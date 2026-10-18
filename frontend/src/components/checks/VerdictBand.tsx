import { useCallback, useContext, useMemo, useState } from 'react'
import type { CheckResult, RunResponse } from '../../types/report'
import type { LoadProgress } from '../../lib/api'
import { LoadBar } from '../LoadBar'
import { ArriveOnce } from '../../motion'
import { Term } from '../ui'
import { plural, readStaticFinding, runView, type Overall } from '../../lib/report-view'
import { namedSpecIn } from '../../lib/ci-workflow'
import { STATUS_COUNT_WORDS, statusOf, TONE_STYLE, useStatusLabel, type ChipTone, type RunStatus, type StatusTerm } from '../../lib/status-vocab'
import { ChipList, Linked } from '../report/ResultLinks'
import type { ResultEntity } from '../../lib/result-links'
import { ResultLinkContext } from '../report/ResultLinks'
import { ShowNetsContext, ToBoard } from './ChecksLinks'
import { ResultChip } from './pieces'
import { blockingCauses, OpenPartsEntry, OpenPartsSheet, openPartCount, useOpenPartsRequest, type RemedyActions } from './Remedies'
import { hasRunDetails, RunResults } from './RunResults'

// The ONE place the Checks page states its verdict. It says the verdict once,
// in a sentence, with the one thing to do next beside it; Run is the page's
// only primary action, and everything that takes the checks elsewhere sits in
// the Export menu next to it.
//
// What the verdict judges: the spec's own checks, plus any finding of the
// board's own static check that fails the circuit (a short, a pin conflict).
// The not-routed state is layout readiness, not a defect in the circuit these
// checks judge, so it is a calm note under the verdict, with the nets it names
// one click from the board. The CLI and CI are unchanged: that same finding
// still fails `hauksbee-ci run` on this spec, and the note says so.

const VERDICT: Record<Overall, { term: StatusTerm | null; tone: ChipTone; fallback: string }> = {
  passed: { term: 'pass', tone: 'ok', fallback: 'PASS' },
  failed: { term: 'fail', tone: 'err', fallback: 'FAIL' },
  invalid: { term: 'invalid', tone: 'warn', fallback: 'INVALID' },
  measured: { term: null, tone: 'quiet', fallback: 'MEASURED' },
}

const INK: Record<ChipTone, string> = {
  ok: 'var(--ok)', err: 'var(--err)', warn: 'var(--warn)', quiet: 'var(--silk-dim)',
}

const cap = (s: string) => (s ? s[0].toUpperCase() + s.slice(1) : s)

/** The board's own check result, as subjects the board can show. */
function entitiesOf(subjects: readonly string[], vocab: { refs: Set<string>; nets: Set<string> } | null): ResultEntity[] {
  return subjects.flatMap((s): ResultEntity[] =>
    vocab?.refs.has(s) ? [{ kind: 'part', ref: s }]
      : vocab?.nets.has(s) || !vocab ? [{ kind: 'net', net: s }] : [])
}

/** Layout readiness: not routed yet. Calm, separate from the verdict. */
function LayoutNote({ finding, notice }: { finding: CheckResult | null; notice: string | null }) {
  const showNets = useContext(ShowNetsContext)
  const read = finding ? readStaticFinding(finding) : null
  const plain = read?.plain ?? notice
  if (!plain) return null
  const nets = read?.subjects ?? []
  return (
    <div data-testid="layout-note" className="mt-3 pt-2.5 text-[12px] leading-relaxed" style={{ borderTop: '1px solid var(--hairline)', color: 'var(--silk-dim)' }}>
      <div className="flex flex-wrap items-baseline gap-x-2 gap-y-1">
        <span className="text-[10px] font-bold tracking-widest uppercase" style={{ color: 'var(--silk-faint)' }}>Layout</span>
        <span className="min-w-0" style={{ color: 'var(--silk)' }}>{cap(plain.replace(/\.$/, ''))}.</span>
      </div>
      <div className="mt-0.5">
        These checks judge the circuit as drawn. A CI run of this spec also fails until the board is routed.
      </div>
      <div className="mt-1.5 flex flex-wrap items-center gap-2">
        {nets.length > 0 && showNets ? (
          <button
            type="button"
            data-testid="layout-show-on-board"
            className="hb-btn hb-press px-2.5 py-1 text-[12px]"
            onClick={() => showNets(nets, plural(nets.length, 'unrouted net'))}
          >
            Show the {plural(nets.length, 'unrouted net')} on the board
          </button>
        ) : (
          <ToBoard>Open the board</ToBoard>
        )}
        {read?.fix && (
          <details className="text-[12px]" data-testid="layout-fix">
            <summary className="cursor-pointer" style={{ color: 'var(--silk-faint)' }}>How to finish the layout</summary>
            <div className="mt-0.5">{read.fix}</div>
          </details>
        )}
      </div>
    </div>
  )
}

/** Board findings that fail the run: judged and counted with the checks. */
function BoardFindings({ results, stale }: { results: readonly CheckResult[]; stale: boolean }) {
  const { vocab } = useContext(ResultLinkContext)
  if (results.length === 0) return null
  return (
    <div data-testid="board-gate" className="mt-3 pt-2.5" style={{ borderTop: '1px solid var(--hairline)' }}>
      <div className="text-[10px] font-bold tracking-widest uppercase" style={{ color: 'var(--silk-faint)' }}>
        Found on the board itself
      </div>
      <ul className="list-none p-0 m-0">
        {results.map((r, i) => {
          const f = readStaticFinding(r)
          const subjects = entitiesOf(f?.subjects ?? [], vocab)
          return (
            <li key={i} data-testid="board-gate-result" className="mt-2 text-[13px]" style={{ color: 'var(--silk)' }}>
              <div className="flex flex-wrap items-start gap-2">
                <ResultChip result={r} stale={stale} />
                <span className="min-w-0" style={{ overflowWrap: 'anywhere' }}><Linked text={cap(f?.plain ?? r.detail)} /></span>
              </div>
              {subjects.length > 0 && <div className="mt-1 text-[12px]"><ChipList entities={subjects} testId="board-gate-subjects" /></div>}
              {r.why && (
                <div className="mt-0.5 text-[12px]" style={{ color: 'var(--silk-dim)' }}>
                  <span className="font-semibold">Fix: </span>{r.why}
                </div>
              )}
            </li>
          )
        })}
      </ul>
    </div>
  )
}

export function VerdictBand({
  result, runKey, stale, running, preparing, progress, onCancel, canRun, onRun, pulseRun,
  validationCount, reportSerious, onJump, specFileName, specToml, rawMode, preLayoutNotice,
  remedyActions, remedyEditor, exportMenu,
}: {
  result: RunResponse | null
  /** Changes only when a new run lands, so the arrival plays once per run. */
  runKey: string | undefined
  stale: boolean
  running: boolean
  preparing?: boolean
  progress?: LoadProgress | null
  onCancel?: () => void
  canRun: boolean
  onRun: () => void
  /** A remedy changed the spec: point at Run, the only thing that re-judges. */
  pulseRun?: boolean
  /** Rows the preflight refused, for the "nothing was run" line. */
  validationCount: number
  /** Serious findings on the board's own report: a pass here does not clear them. */
  reportSerious: number
  /** Jump to the first check with this status, inline in the active surface. */
  onJump: (status: RunStatus) => void
  /** The spec file the page downloads; named in an engine message's placeholder. */
  specFileName: string
  /** The spec text the run was made from, which says which results are rows. */
  specToml: string
  /** No builder rows to annotate (TOML mode, or kept-as-written asserts). */
  rawMode: boolean
  /** The board report's own not-routed notice, for before a run. */
  preLayoutNotice: string | null
  remedyActions?: RemedyActions
  remedyEditor?: React.ReactNode
  exportMenu: React.ReactNode
}) {
  const label = useStatusLabel()
  const [detailsOpen, setDetailsOpen] = useState(false)
  const [sheetOpen, setSheetOpen] = useState(false)
  useOpenPartsRequest(useCallback(() => setSheetOpen(true), []))
  const ok = result?.ok ? result : null
  const view = useMemo(() => (ok ? runView(ok, specToml) : null), [ok, specToml])
  const groups = useMemo(() => (ok ? blockingCauses(ok) : []), [ok])
  const openParts = openPartCount(groups)
  const working = running || !!preparing

  const counts: Record<RunStatus, number> = { pass: 0, fail: 0, invalid: 0, measured: 0 }
  for (const r of view?.judged ?? []) counts[statusOf(r)] += 1
  const total = view?.judged.length ?? 0
  const verdict = view?.verdict ?? null
  const conditional = verdict === 'passed' && (ok?.verdict_conditions ?? []).some(c => c.trim() !== '')
  const uncovered = verdict === 'passed' && reportSerious > 0

  let tone: ChipTone = 'quiet'
  let word: React.ReactNode = null
  let line: React.ReactNode
  let next: React.ReactNode = null
  if (running) line = 'Running the checks…'
  else if (preparing) line = 'Preparing the checks for this board…'
  else if (result && !result.ok) {
    tone = 'err'
    line = <span style={{ overflowWrap: 'anywhere' }}>{namedSpecIn(result.error, specFileName)}</span>
  } else if (verdict && view) {
    const v = VERDICT[uncovered ? 'invalid' : verdict]
    tone = v.tone
    const w = v.term ? label(v.term) : v.fallback
    word = v.term ? <Term id={v.term} bare><span>{w}</span></Term> : w
    const checks = (n: number) => plural(n, 'check')
    if (verdict === 'failed') {
      line = `${counts.fail} of ${checks(total)} ${counts.fail === 1 ? 'fails' : 'fail'}.`
      const firstRow = view.rows.findIndex(r => statusOf(r) === 'fail')
      if (firstRow >= 0) next = <button type="button" data-testid="verdict-next" className="hb-chip hb-press px-2.5 py-1 text-[12px]" onClick={() => onJump('fail')}>Show the first failure</button>
    } else if (verdict === 'invalid') {
      line = openParts > 0
        ? `Not judged yet: ${counts.invalid === 1 ? '1 check waits' : `${counts.invalid} checks wait`} on models for ${plural(openParts, 'part')}.`
        : `${counts.invalid} of ${checks(total)} could not be judged.`
      if (remedyActions) next = <OpenPartsEntry groups={groups} blocking onOpen={() => setSheetOpen(true)} />
    } else if (verdict === 'measured') {
      line = 'Measured only: no check sets a bound, so nothing was judged.'
    } else if (uncovered) {
      line = <>{total === 1 ? 'The check passes' : `All ${checks(total)} pass`}, but {plural(reportSerious, 'serious issue')} in the board report {reportSerious === 1 ? 'is' : 'are'} not covered by {total === 1 ? 'it' : 'them'}. <ToBoard>Open the board report</ToBoard></>
    } else {
      line = `${total === 1 ? 'The check passes' : `All ${checks(total)} pass`}${conditional ? ', on a condition (see Details)' : ''}.`
      if (conditional && remedyActions && groups.length > 0) next = <OpenPartsEntry groups={groups} blocking={false} onOpen={() => setSheetOpen(true)} />
    }
  } else {
    line = canRun ? 'Not run yet. Run the checks to judge each one below.' : 'Running from here needs the board uploaded in this session.'
  }
  const style = tone === 'quiet' ? undefined : { boxShadow: `inset 4px 0 0 ${INK[tone]}` }
  const details = ok && verdict ? hasRunDetails(ok, verdict, rawMode) : false
  const showCounts = !!view && !working && (Object.values(counts).filter(n => n > 0).length > 1 || total > 1 || rawMode)

  return (
    <section data-testid="run-summary" aria-live="polite" className="hb-card px-4 py-3" style={style}>
      <div className="flex flex-wrap items-start gap-x-4 gap-y-2.5">
        <div className="min-w-0 flex-1" style={{ minWidth: 'min(100%, 260px)', opacity: stale && view ? 0.6 : 1 }}>
          <div className="text-[15px] font-semibold leading-snug" data-testid="run-headline" style={{ color: 'var(--silk)' }}>
            {word && <span className="mr-2 text-[12px] font-bold tracking-wider px-2 py-0.5 rounded-full align-[2px]" data-testid="run-tag" style={TONE_STYLE[tone]}>{word}</span>}
            {line}
            {ok?.analog_abort ? ' The analog solve aborted, so these results are not trustworthy.' : ''}
          </div>
          {stale && result && !working && (
            <div className="text-[12px] mt-0.5" style={{ color: 'var(--silk-dim)' }}>From the last run; the spec has changed since. Run again to judge it.</div>
          )}
        </div>
        <div className="flex items-center gap-2 shrink-0">
          <button
            type="button"
            data-testid="run-checks"
            disabled={!canRun || running || preparing}
            onClick={onRun}
            className={`hb-btn-primary hb-press px-4 py-1.5 text-[13px] inline-flex items-center gap-2${pulseRun && !running ? ' run-nudge' : ''}`}
          >
            {running && <span className="slot-spin" style={{ borderTopColor: 'var(--on-copper)' }} />}
            {running ? 'Running…' : preparing ? 'Preparing…' : result || pulseRun ? 'Run again' : 'Run checks'}
          </button>
          {exportMenu}
        </div>
      </div>

      {(next || showCounts || (result && details)) && !working && (
        <div className="mt-2 flex flex-wrap items-center gap-x-2 gap-y-1.5">
          {next}
          {showCounts && (['fail', 'invalid', 'pass', 'measured'] as const).filter(s => counts[s] > 0).map(s => (
            <button
              key={s}
              type="button"
              data-testid={`run-count-${s}`}
              onClick={() => onJump(s)}
              title="Jump to the first one"
              className="hb-press px-1.5 py-0.5 text-[12px] tnum underline underline-offset-2"
              style={{ background: 'none', border: 'none', color: 'var(--silk-dim)' }}
            >
              {counts[s]} {STATUS_COUNT_WORDS[s]}
            </button>
          ))}
          {details && (
            <button
              type="button"
              data-testid="run-details-toggle"
              aria-expanded={detailsOpen}
              onClick={() => setDetailsOpen(o => !o)}
              className="hb-press text-[12px] cursor-pointer ml-auto underline underline-offset-2"
              style={{ background: 'none', border: 'none', color: 'var(--silk-faint)' }}
            >
              {detailsOpen ? 'Hide details' : 'Details'}
            </button>
          )}
        </div>
      )}

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
            <button type="button" data-testid="run-cancel" onClick={onCancel} className="hb-btn hb-press px-3 py-1 text-[12px]">
              {running ? 'Cancel the run' : 'Skip preparing'}
            </button>
          )}
        </div>
      )}

      {validationCount > 0 && (
        <div data-testid="builder-validation" className="text-[12px] mt-1.5" style={{ color: 'var(--err-strong)' }}>
          Nothing was run: {validationCount === 1 ? 'a check is' : `${validationCount} checks are`} missing
          values. The highlighted fields say which.
        </div>
      )}

      {ok && detailsOpen && details && verdict && (
        <ArriveOnce key={runKey} className="mt-2">
          <div data-testid="run-details" className="pt-2" style={{ borderTop: '1px solid var(--hairline)' }}>
            <RunResults result={ok} overall={verdict} stale={stale} rawMode={rawMode} />
          </div>
        </ArriveOnce>
      )}

      {view && <BoardFindings results={view.boardFindings} stale={stale} />}
      {!working && <LayoutNote finding={view?.layout ?? null} notice={view ? null : preLayoutNotice} />}

      {/* The model editor a remedy opened stays reachable with the sheet shut. */}
      {!sheetOpen && remedyEditor}
      {sheetOpen && remedyActions && (
        <OpenPartsSheet groups={groups} actions={remedyActions} editor={remedyEditor} onClose={() => setSheetOpen(false)} />
      )}
    </section>
  )
}
