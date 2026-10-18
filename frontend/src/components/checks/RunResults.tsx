import { useEffect, useRef, useState } from 'react'
import type { RunResponse } from '../../types/report'
import { explainRun, overallVerdict, plural, repeatedAssumptions, type Overall } from '../../lib/report-view'
import { namedSpecIn } from '../../lib/ci-workflow'
import { ArriveOnce } from '../../motion'
import { DownloadIcon } from '../Icons'
import { useGlossary } from '../ui'
import { ExplainVerdict } from '../report/VerdictConditions'
import { AssertionEvidence, parseMeasures, ResultChip } from './pieces'
import { OpenParts, type RemedyActions } from './Remedies'

// The run, summed up in one strip: the verdict, the counts, the one entry to
// what blocks judgement, and the export. Each check's own result lives on its
// row (tag, measured-vs-limit bar, show on board), so nothing is listed twice.

const TONE: Record<Overall, { ink: string; bg: string; border: string; tag: string }> = {
  passed: { ink: 'var(--ok)', bg: 'var(--ok-bg)', border: 'var(--ok-border)', tag: 'PASS' },
  measured: { ink: 'var(--silk-dim)', bg: 'var(--surface-2)', border: 'var(--hairline)', tag: 'MEASURED' },
  invalid: { ink: 'var(--warn)', bg: 'var(--warn-bg)', border: 'var(--warn-border)', tag: 'NOT JUDGED' },
  failed: { ink: 'var(--err)', bg: 'var(--err-bg)', border: 'var(--err-border)', tag: 'FAIL' },
}

const VERDICT_LINE: Record<Overall, string> = {
  passed: 'All checks passed.',
  measured: 'Measured only: no bound was set, so nothing was judged.',
  invalid: 'Some checks could not be judged.',
  failed: 'Checks failed.',
}

/** The three files `hauksbee-ci run --json --junit --emit-manifest` wrote for
 *  this run, served byte for byte by the export route. */
const EXPORTS: [file: string, label: string, what: string][] = [
  ['results.xml', 'JUnit XML', 'for a CI test report'],
  ['report.json', 'Run as JSON', 'what hauksbee-ci run --json prints'],
  ['manifest.json', 'Run manifest', 'inputs and versions, to reproduce it'],
]

function RunExport({ id }: { id: string }) {
  const [open, setOpen] = useState(false)
  const wrap = useRef<HTMLDivElement>(null)
  useEffect(() => {
    if (!open) return
    const away = (e: MouseEvent) => { if (!wrap.current?.contains(e.target as Node)) setOpen(false) }
    const esc = (e: KeyboardEvent) => { if (e.key === 'Escape') setOpen(false) }
    document.addEventListener('mousedown', away)
    document.addEventListener('keydown', esc)
    return () => { document.removeEventListener('mousedown', away); document.removeEventListener('keydown', esc) }
  }, [open])
  return (
    <div ref={wrap} className="relative" data-testid="run-export">
      <button
        type="button"
        data-testid="run-export-open"
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => setOpen(o => !o)}
        className="hb-btn hb-press inline-flex items-center gap-1.5 px-2.5 text-[12px] whitespace-nowrap"
        style={{ height: 30 }}
      >
        <DownloadIcon size={13} /> Export run
      </button>
      {open && (
        <div role="menu" className="absolute right-0 z-30 mt-1 rounded-lg py-1" style={{ width: 'min(260px, 80vw)', background: 'var(--surface)', border: '1px solid var(--hairline)', boxShadow: 'var(--shadow-pop)' }}>
          {EXPORTS.map(([file, label, what]) => (
            <a
              key={file}
              role="menuitem"
              data-testid={`run-export-${file}`}
              href={`/api/ci/export/${encodeURIComponent(id)}/${file}`}
              download={`hauksbee-checks-${file}`}
              onClick={() => setOpen(false)}
              className="block px-3 py-1.5 hb-press"
              style={{ color: 'var(--silk)', textDecoration: 'none' }}
            >
              <div className="text-[12px] font-semibold">{label}</div>
              <div className="text-[11px]" style={{ color: 'var(--silk-faint)' }}>{what}</div>
            </a>
          ))}
        </div>
      )}
    </div>
  )
}

/** Overall result. It fades up once when the run lands; it does NOT re-animate
 *  when the spec is edited afterwards, because the panel then goes stale rather
 *  than becoming new. */
export function RunResults({ result, runKey, stale, rawMode, reportSerious, remedyActions, remedyEditor, specFileName }: {
  result: RunResponse
  /** The spec file the page downloads; named in an engine message's placeholder. */
  specFileName: string
  /** Changes only when a new run lands, so the arrival plays once per run. */
  runKey: string | undefined
  stale: boolean
  /** Raw mode has no builder rows to annotate, so results are listed here. */
  rawMode: boolean
  /** Serious findings the board's own report raised. A spec that asserts
   *  nothing about them has not cleared them, so a pass here is not green. */
  reportSerious: number
  /** What a remedy button does here; without it the open-parts entry is hidden. */
  remedyActions?: RemedyActions
  /** The model editor a remedy opened, shown in the open-parts sheet. */
  remedyEditor?: React.ReactNode
}) {
  const results = result.ok ? result.results ?? [] : []
  const overall = overallVerdict(result)
  const repeats = repeatedAssumptions(results)
  const uncovered = overall === 'passed' && reportSerious > 0
  useGlossary(result.glossary)
  // A pass that rests on open parts the member sweep could not cross in full.
  const conditions = (result.verdict_conditions ?? []).filter(c => c.trim() !== '')
  const conditional = overall === 'passed' && conditions.length > 0
  const explanation = result.ok ? explainRun(result, overall, conditional) : null
  const tone = TONE[uncovered ? 'invalid' : overall]
  const failed = results.filter(x => !x.invalid && !x.passed && !x.measured).length
  const invalid = results.filter(x => x.invalid).length
  const passed = results.filter(x => x.passed && !x.invalid).length
  const notes = [
    ...(result.substitutions ?? []).map(s => `Substitute core: ${s}`),
    ...(result.coverage_warnings ?? []).map(s => `Coverage: ${s}`),
    ...(result.timing_coverage ?? []).map(t =>
      `Timing on ${t.mcu_ref} (${t.backend}): edges land within ±${(t.timestamp_precision_s * 1e6).toFixed(3)} µs, and pulses of ${(t.minimum_guaranteed_pulse_s * 1e6).toFixed(3)} µs or longer are always seen${t.cycle_exact ? ' (cycle-exact)' : ' (sampled at poll boundaries)'}.`),
  ]
  return (
    <ArriveOnce key={runKey} className="mt-3">
      <div data-testid="check-results" data-verdict={overall} aria-live="polite">
        {result.ok === false ? (
          // The server's vocabulary errors list the accepted values as one
          // unbroken pipe-separated run ("voltage|uart|..."), which has no
          // space to break at; `anywhere` is what lets a single long token wrap.
          <div
            className="rounded-lg px-3 py-2.5 text-[13px]"
            style={{ background: 'var(--err-bg)', border: '1px solid var(--err-border)', color: 'var(--err-strong)', overflowWrap: 'anywhere' }}
          >
            {namedSpecIn(result.error, specFileName)}
          </div>
        ) : (
          <div className="rounded-xl px-3.5 py-3" data-testid="run-summary" style={{ background: tone.bg, border: `1px solid ${tone.border}` }}>
            <div className="flex flex-wrap items-baseline justify-between gap-x-3 gap-y-1">
              <span className="text-[11px] font-bold tracking-widest" data-testid="run-tag" style={{ color: tone.ink }}>{tone.tag}</span>
              {results.length > 0 && (
                <span className="text-[12px] tnum" data-testid="run-counts" style={{ color: 'var(--silk-dim)' }}>
                  {[passed > 0 && `${passed} passed`, failed > 0 && `${failed} failed`, invalid > 0 && `${invalid} not judged`].filter(Boolean).join(' · ') || plural(results.length, 'check')}
                </span>
              )}
            </div>
            <div className="mt-0.5 text-[14px] font-semibold" style={{ color: 'var(--silk)' }}>
              {uncovered ? 'These checks passed, but the board is not cleared.'
                : conditional ? 'All checks passed, on a condition.' : VERDICT_LINE[overall]}
              {result.analog_abort && ' The analog solve aborted, so these results are not trustworthy.'}
            </div>
            {uncovered && (
              <div className="text-[12px] mt-0.5" data-testid="checks-uncovered-serious" style={{ color: 'var(--warn-strong)' }}>
                {plural(reportSerious, 'serious issue')} in the board report {reportSerious === 1 ? 'is' : 'are'} not
                covered by these checks, so this run does not clear the board.
              </div>
            )}
            {overall === 'failed' && result.verdict_reason && (
              <div className="mt-0.5 text-[13px]" data-testid="checks-verdict-reason" style={{ color: 'var(--silk)' }}>
                {result.verdict_reason}
              </div>
            )}
            {conditions.map((c, i) => (
              <div key={i} className="mt-0.5 text-[12px]" data-testid="run-condition" style={{ color: 'var(--warn-strong)' }}>On a condition: {c}</div>
            ))}
            {(result.timing_refusals ?? []).map((t, i) => (
              <div key={i} className="mt-0.5 text-[12px]" style={{ color: 'var(--err)' }}>Timing not judged: {t}</div>
            ))}
            {stale && (
              <div className="text-[12px] mt-0.5" style={{ color: 'var(--silk-dim)' }}>From a previous run; the spec has changed since.</div>
            )}
            <div className="mt-2 flex flex-wrap items-center gap-1.5">
              <OpenParts result={result} blocking={overall === 'invalid'} actions={remedyActions} editor={remedyEditor} />
              {result.export_id && <RunExport id={result.export_id} />}
            </div>
            {explanation && <ExplainVerdict explanation={explanation} open={false} />}
            {notes.length > 0 && (
              <details className="mt-1.5 text-[12px]" data-testid="run-notes" style={{ color: 'var(--silk-dim)' }}>
                <summary className="cursor-pointer" style={{ color: 'var(--silk-faint)' }}>Run notes ({notes.length})</summary>
                {notes.map((n, i) => <div key={i} className="mt-1">{n}</div>)}
              </details>
            )}
          </div>
        )}
        {result.ok && rawMode && results.map((x, i) => (
          <div key={i} className="mt-1.5 rounded-lg px-3 py-2 text-[13px]" style={{ background: 'var(--surface-2)', border: '1px solid var(--hairline)' }}>
            <div className="flex flex-wrap items-center gap-2">
              <ResultChip result={x} stale={stale} />
              <span style={{ color: 'var(--silk)' }}>{x.label}</span>
              {parseMeasures(x.detail).length === 0 && <span style={{ color: 'var(--silk-dim)' }}>{x.detail}</span>}
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
    </ArriveOnce>
  )
}
