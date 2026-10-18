import { useContext, useMemo } from 'react'
import type { ArtifactProvenance, CheckResult, EvidenceAssumption } from '../../types/report'
import { assumptionsForEvidence, evidenceStatusTerm, modelProvenanceLines, plural } from '../../lib/report-view'
import type { ResultEntity } from '../../lib/result-links'
import { EntityChip, ResultLinkContext } from '../report/ResultLinks'
import { causeGroups, openPartCount, showOpenParts, withoutIds } from './Remedies'
import { summarizeErrorBudget } from '../../lib/report-view'
import { VerdictBadge } from '../../motion'
import { Term } from '../ui'

// The small pieces the Checks view repeats: a labelled input, the PASS/FAIL
// chip that rides on a row, and the evidence block under it.

/** One labelled builder input. `width` is what the value wants; on a narrow
 *  column it is a ceiling and the field takes what is left rather than pushing
 *  the row off the card. */
export function Field({ label, value, onChange, width = 90, placeholder, invalid, list, testId }: {
  label: string
  value: string
  onChange: (v: string) => void
  width?: number
  placeholder?: string
  /** Highlight as the offending input of a validation message. */
  invalid?: boolean
  /** Id of a datalist to complete from (the shared net picker). */
  list?: string
  testId?: string
}) {
  return (
    <label
      className="inline-flex items-center gap-1.5 text-[12px] min-w-0 max-w-full"
      style={{ color: invalid ? 'var(--err)' : 'var(--silk-faint)' }}
    >
      {label}
      <input
        className={`hb-input tnum min-w-0${list ? ' flex-1' : ''}`}
        aria-invalid={invalid || undefined}
        data-testid={testId}
        list={list}
        style={{
          // With a picker the field grows into the row and `width` caps it;
          // without one `width` is what the value wants, capped by the column.
          ...(list ? { maxWidth: width } : { width, maxWidth: '100%' }),
          ...(invalid ? { borderColor: 'var(--err)', background: 'var(--err-bg)' } : {}),
        }}
        value={value}
        placeholder={placeholder}
        onChange={e => onChange(e.target.value)}
      />
    </label>
  )
}

/** Inline PASS / FAIL / INVALID / MEASURED chip riding on a check row. */
export function ResultChip({ result, stale }: { result: CheckResult; stale: boolean }) {
  const tone = result.invalid
    ? { color: 'var(--warn)', bg: 'var(--warn-bg)', border: 'var(--warn-border)', label: 'INVALID', term: 'invalid' }
    : result.measured
    ? { color: 'var(--silk-faint)', bg: 'var(--surface-2)', border: 'var(--hairline)', label: 'MEASURED' }
    : result.passed
      ? { color: 'var(--ok)', bg: 'var(--ok-bg)', border: 'var(--ok-border)', label: 'PASS', term: 'pass' }
      : { color: 'var(--err)', bg: 'var(--err-bg)', border: 'var(--err-border)', label: 'FAIL', term: 'fail' }
  // The word swaps (PASS becoming FAIL on a re-run) inside a fixed grid cell,
  // so the row never reflows on a verdict change. The colour is not animated:
  // it is applied by the wrapper's style, which means the chip is the right
  // colour on its first painted frame. Hover or focus explains the word.
  const badge = (
    <VerdictBadge
      data-testid="row-result"
      label={tone.label}
      title={stale ? `${result.detail} (from the last run; the spec has changed since)` : result.detail}
      className="px-2 rounded-full text-[10px] font-bold tracking-wide"
      style={{
        background: tone.bg, border: `1px solid ${tone.border}`, color: tone.color,
        height: 20, opacity: stale ? 0.45 : 1, flexShrink: 0,
      }}
    />
  )
  return tone.term ? <Term id={tone.term} bare>{badge}</Term> : badge
}

/** One measured quantity a check's detail line reports, with its limits. */
export interface Measure {
  /** The measured values (a window's min and max, or one reading). */
  values: number[]
  lo?: number
  hi?: number
  unit: string
}

const NUM = String.raw`(-?\d+(?:\.\d+)?)`
const all = (text: string, src: string) => [...text.matchAll(new RegExp(src, 'g'))]

/** The numbers in a check's detail line, as measured-vs-limit pairs. The
 *  runner writes them as prose (`max=0.000V (<= 0.75V)`, `3 toggles (need >=
 *  2)`, `boot 1.20ms (<= 2ms)`); this reads only those shapes and returns
 *  nothing for any other, so an unknown line keeps its text alone. */
export function parseMeasures(detail: string): Measure[] {
  const out: Measure[] = []
  // Window statistics: min/max, passing `(>= lo)` or failing `< required lo`.
  const stat: Measure = { values: [], unit: '' }
  for (const [, which, v, unit, okLimit, badLimit] of all(detail,
    String.raw`\b(min|max)=${NUM}\s*([A-Za-zµ°]*)\s*(?:\((?:>=|<=)\s*${NUM}|[<>]\s*(?:required|allowed)\s*${NUM})`)) {
    stat.values.push(Number(v))
    stat.unit ||= unit
    if (which === 'min') stat.lo = Number(okLimit ?? badLimit)
    else stat.hi = Number(okLimit ?? badLimit)
  }
  if (stat.values.length > 0) out.push(stat)
  for (const [, n, need] of all(detail, String.raw`(\d+) toggles \(need >= (\d+)\)`)) {
    out.push({ values: [Number(n)], lo: Number(need), unit: 'toggles' })
  }
  for (const [, v, unit, okLimit, badLimit] of all(detail,
    String.raw`${NUM}\s*(ms|A|C)\s*(?:\(<=\s*${NUM}\s*\2\)|>\s*allowed\s*${NUM}\s*\2)`)) {
    out.push({ values: [Number(v)], hi: Number(okLimit ?? badLimit), unit })
  }
  // A peripheral field: `id.field = v (>= lo, <= hi)`.
  for (const [, v, bounds] of all(detail, String.raw`=\s*${NUM}\s*\(((?:[<>]=\s*-?[\d.]+(?:,\s*)?)+)\)`)) {
    const lo = />=\s*(-?[\d.]+)/.exec(bounds)
    const hi = /<=\s*(-?[\d.]+)/.exec(bounds)
    out.push({ values: [Number(v)], lo: lo ? Number(lo[1]) : undefined, hi: hi ? Number(hi[1]) : undefined, unit: '' })
  }
  return out
}

const fmt = (n: number) => (Number.isInteger(n) || Math.abs(n) >= 100 ? String(n) : String(Number(n.toPrecision(3))))

/** Measured against allowed: the allowed band shaded, the measured value (or
 *  its min..max span) marked in the verdict's colour, the numbers beside it. */
export function MeasureBar({ measure, ok }: { measure: Measure; ok: boolean }) {
  const { values, lo, hi, unit } = measure
  const bounds = [lo, hi].filter((b): b is number => b !== undefined)
  const vMin = Math.min(...values)
  const vMax = Math.max(...values)
  const pts = [...values, ...bounds]
  const span = Math.max(...pts) - Math.min(...pts) || Math.abs(vMax) || 1
  // A one-sided ceiling on a non-negative reading starts the scale at zero.
  const min = lo === undefined && vMin >= 0 ? 0 : Math.min(...pts) - span * 0.25
  const max = Math.max(...pts) + span * 0.25
  const W = 160
  const x = (v: number) => 4 + ((v - min) / (max - min || 1)) * (W - 8)
  const bandLo = x(lo ?? min)
  const bandHi = x(hi ?? max)
  const tone = ok ? 'var(--ok)' : 'var(--err)'
  const u = unit === 'toggles' ? ' toggles' : unit ? ` ${unit}` : ''
  const measured = values.length > 1 && vMin !== vMax ? `${fmt(vMin)} to ${fmt(vMax)}${u}` : `${fmt(vMax)}${u}`
  const limit = lo !== undefined && hi !== undefined ? `${fmt(lo)} to ${fmt(hi)}${u}`
    : lo !== undefined ? `at least ${fmt(lo)}${u}` : hi !== undefined ? `at most ${fmt(hi)}${u}` : ''
  return (
    <span className="inline-flex flex-wrap items-center gap-x-2 gap-y-0.5 tnum" data-testid="measure-bar">
      <svg width={W} height="14" viewBox={`0 0 ${W} 14`} role="img" aria-label={`measured ${measured}, allowed ${limit}`} style={{ flexShrink: 0, maxWidth: '100%' }}>
        <rect x="4" y="6" width={W - 8} height="2" rx="1" fill="var(--hairline)" />
        <rect x={bandLo} y="3" width={Math.max(bandHi - bandLo, 2)} height="8" rx="2" fill="var(--ok-bg)" stroke="var(--ok-border)" />
        {x(vMax) - x(vMin) > 3
          ? <rect x={x(vMin)} y="4.5" width={x(vMax) - x(vMin)} height="5" rx="2.5" fill={tone} />
          : <circle cx={x(vMax)} cy="7" r="4" fill={tone} />}
      </svg>
      <span style={{ color: 'var(--silk)' }}>{measured}</span>
      {limit && <span style={{ color: 'var(--silk-faint)' }}>allowed {limit}</span>}
    </span>
  )
}

/** The part or net a check's detail is about, for its "show on board" chip. */
function subjectOf(detail: string): ResultEntity | null {
  const part = /\b(?:I|Tj)\(([A-Za-z]+\d+[A-Za-z0-9]*)\)/.exec(detail)
  if (part) return { kind: 'part', ref: part[1] }
  const net = /^([^\s:]+?)(?::| window:)/.exec(detail)
  return net ? { kind: 'net', net: net[1] } : null
}

/** A check row's result, inline: the measured-vs-limit bar, a chip that shows
 *  the subject on the board, and for a check that could not be judged one line
 *  saying what blocks it, with the button that opens the open-parts sheet. The
 *  registry's own sentences, inputs and numbers are one click away, verbatim,
 *  so web, CLI and CI explain a limitation the same way. */
export function AssertionEvidence({ result, assumptions, inventory, repeats, stale, part = 'all' }: {
  result: CheckResult
  assumptions: readonly EvidenceAssumption[]
  inventory: readonly ArtifactProvenance[]
  repeats: ReadonlySet<string>
  stale: boolean
  /** 'glance': what a row always shows (bars, the board link, why it was not
   *  judged); 'details': the evidence behind it, unfolded; 'all': both, with
   *  the evidence folded. */
  part?: 'all' | 'glance' | 'details'
}) {
  const { onGo } = useContext(ResultLinkContext)
  const map = result.evidence
  const measures = useMemo(() => parseMeasures(result.detail), [result.detail])
  const subject = useMemo(() => subjectOf(result.detail), [result.detail])
  const linked = map ? assumptionsForEvidence(map, assumptions) : []
  const artifacts = (map?.artifacts ?? []).flatMap(index => inventory[index] ? [inventory[index]] : [])
  const open = openPartCount(causeGroups(assumptions))
  const why = !result.invalid ? null
    : result.kind === 'no_faults' && open > 0
      ? `this whole-board check is judged once every part has a model, so it waits on the ${plural(open, 'open part')}. That is expected on a first run.`
      : linked[0] ? withoutIds(linked[0].statement) : null
  const status = map?.status ?? 'clean'
  const details = linked.length > 0 || artifacts.length > 0 || !!map?.error_budget || modelProvenanceLines(map).length > 0
  const showSubject = !!subject && !!onGo
  const glance = part !== 'details'
  const unfold = part !== 'glance'
  const note = part === 'glance' && measures.length === 0 && !why ? result.detail : ''
  if (!(glance && (measures.length > 0 || showSubject || why || note)) && !(unfold && details)) return null
  const evidence = <>
    {linked.map(a => (
      <div key={a.id} className="mt-1.5">
        <div style={{ color: 'var(--silk)' }}>{withoutIds(a.statement)}{a.term && <> <Term id={a.term} /></>}</div>
        {repeats.has(a.id) ? '(explained above)' : <>
          <div>{withoutIds(`${a.because} ${a.consequence}`)}</div>
          <div>To remove it: {withoutIds(a.replacement)}</div>
        </>}
      </div>
    ))}
    {artifacts.length > 0 && (
      <div className="mt-1.5" style={{ color: 'var(--silk-faint)' }}>
        Inputs: {artifacts.map(artifact => artifact.path.split('/').pop() ?? artifact.path).join(', ')}
      </div>
    )}
    {map?.error_budget && (
      <div className="mt-1" data-testid="error-budget-summary">
        {summarizeErrorBudget(map.error_budget).map(row => <div key={row}>{row}</div>)}
      </div>
    )}
    {modelProvenanceLines(map).length > 0 && (
      <div className="mt-1" data-testid="model-provenance" style={{ color: 'var(--silk-faint)' }}>
        Models: {modelProvenanceLines(map).join('; ')}
      </div>
    )}
  </>
  const statusWord = <Term id={evidenceStatusTerm(status)}>{status}</Term>
  return (
    <div data-testid="assertion-evidence" className="mt-1.5 text-[12px]" style={{ color: 'var(--silk-dim)', opacity: stale ? 0.55 : 1 }}>
      {note && <div data-testid="result-note">{note}{stale ? ' (from the last run; the spec has changed since)' : ''}</div>}
      {glance && (measures.length > 0 || showSubject) && (
        <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
          {measures.map((m, i) => <MeasureBar key={i} measure={m} ok={result.passed && !result.invalid} />)}
          {showSubject && subject && <span data-testid="show-on-board"><EntityChip entity={subject} /></span>}
        </div>
      )}
      {glance && why && (
        <div className="mt-1 flex flex-wrap items-center gap-x-2 gap-y-1" data-testid="not-judged" style={{ color: 'var(--warn-strong)' }}>
          <span>Not judged: {why}</span>
          {open > 0 && (
            <button type="button" className="hb-chip hb-press px-2 text-[12px]" style={{ height: 24 }} data-testid="not-judged-open" onClick={showOpenParts}>
              Fix what blocks it
            </button>
          )}
        </div>
      )}
      {unfold && details && (part === 'details' ? (
        <div className="mt-1" data-testid="numeric-details">
          <div style={{ color: status === 'undermined' ? 'var(--warn)' : 'var(--silk-faint)' }}>Evidence {statusWord}</div>
          {evidence}
        </div>
      ) : (
        <details className="mt-1" data-testid="numeric-details">
          <summary className="cursor-pointer" style={{ color: status === 'undermined' ? 'var(--warn)' : 'var(--silk-faint)' }}>
            Evidence {statusWord}
          </summary>
          {evidence}
        </details>
      ))}
    </div>
  )
}

/** A section card in the builder column: title, optional caption, and the
 *  controls that add rows to it. */
export function BuilderSection({ title, caption, actions, testId, children }: {
  title: string
  caption?: React.ReactNode
  actions?: React.ReactNode
  testId?: string
  children: React.ReactNode
}) {
  return (
    <section className="hb-card px-4 py-3.5 mb-4" data-testid={testId}>
      <div className="flex flex-wrap items-center justify-between gap-2 mb-2">
        <div>
          <h2 className="text-[11px] font-bold tracking-widest uppercase" style={{ margin: 0, color: 'var(--silk-faint)' }}>
            {title}
          </h2>
          {caption && (
            <div className="text-[11px] mt-1" style={{ color: 'var(--silk-faint)' }}>{caption}</div>
          )}
        </div>
        {actions && <div className="flex flex-wrap gap-1.5">{actions}</div>}
      </div>
      {children}
    </section>
  )
}

/** A row's `remove` affordance: the same quiet text button everywhere. */
export function RemoveButton({ onClick, label = 'remove', className = '' }: {
  onClick: () => void
  label?: string
  className?: string
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={`hb-press text-[11px] cursor-pointer ${className}`}
      style={{ color: 'var(--silk-faint)', background: 'none', border: 'none' }}
    >
      {label}
    </button>
  )
}

/** The copper "+ something" text button used inside a row. */
export function AddInlineButton({ onClick, children }: { onClick: () => void; children: React.ReactNode }) {
  return (
    <button
      type="button"
      onClick={onClick}
      className="hb-press text-[11px] cursor-pointer"
      style={{ color: 'var(--copper)', background: 'none', border: 'none' }}
    >
      {children}
    </button>
  )
}

/** The bordered well one builder row sits in; red-edged while it has issues. */
export function RowCard({ testId, invalid, children }: {
  testId: string
  invalid: boolean
  children: React.ReactNode
}) {
  return (
    <div
      className="rounded-lg px-3 py-3 mb-2"
      data-testid={testId}
      style={{ background: 'var(--surface-2)', border: `1px solid ${invalid ? 'var(--err)' : 'var(--hairline)'}` }}
    >
      {children}
    </div>
  )
}

export function RowIssues({ issues }: { issues: string[] }) {
  return (
    <>
      {issues.map(issue => (
        <div key={issue} className="text-[11px] mt-1" style={{ color: 'var(--err-strong)' }}>{issue}</div>
      ))}
    </>
  )
}
