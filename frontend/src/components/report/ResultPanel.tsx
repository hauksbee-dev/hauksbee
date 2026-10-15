// The board result: one panel that says the verdict, shows how much of the
// board it covers, itemises what qualifies it, and leads from each item to the
// board, the model flow, the checks and an export.
//
//   ┌ [glyph] VERDICT TAG                                  board · parts · nets ┐
//   │ Title (the engine headline's lead)                                       │
//   │ the rest of the headline                                                 │
//   │ (ring) net checks judged  (ring) key parts modelled  (n) serious · found │
//   ├ rows: partly judged scope / conditions / findings, with actions ─────────┤
//   └ [Show result on board] [Export ▾] [Copy CLI command] [Open Checks] ──────┘
//
// The words are the engine's: the tag is what `VERDICT: <tag>:` prints, the
// title and subtitle are the headline cut at its first colon, and every row is
// a sentence or a field of the report. Nothing here re-judges the board.

import { useContext } from 'react'
import type { WebReport } from '../../types/report'
import type { FindingGroup, ReportView, VerdictExplanation } from '../../lib/report-view'
import { plural } from '../../lib/report-view'
import { verdictTag } from '../../lib/result-export'
import { entitiesIn } from '../../lib/result-links'
import { CheckIcon, ChecksIcon, CloseIcon, TerminalIcon, WarningIcon } from '../Icons'
import { CopyButton } from '../ui'
import { ExplainVerdict, PartlyJudged, ResultRow, VerdictConditions } from './VerdictConditions'
import { AddCheck, Linked, ResultLinkContext, RowAction } from './ResultLinks'

/** The headline as a title and the rest: cut at the first colon, else after
 *  the first sentence. A conditional headline's trailing "this rests on:" is
 *  dropped because the condition rows follow. */
export function splitHeadline(headline: string): { title: string; rest: string } {
  const colon = headline.indexOf(': ')
  const sentence = headline.search(/[.!?]\s+[A-Z]/)
  let title = headline, rest = ''
  if (colon > 0 && (sentence < 0 || colon < sentence)) {
    title = headline.slice(0, colon)
    rest = headline.slice(colon + 2)
  } else if (sentence > 0) {
    title = headline.slice(0, sentence + 1)
    rest = headline.slice(sentence + 1).trim()
  }
  rest = rest.replace(/;?\s*this rests on:?$/i, '').trim()
  return { title, rest }
}

const TONE = {
  ok: { ink: 'var(--ok)', glyph: <CheckIcon size={16} /> },
  warning: { ink: 'var(--warn)', glyph: <WarningIcon size={16} /> },
  error: { ink: 'var(--err)', glyph: <CloseIcon size={16} /> },
} as const

/** A ring gauge: `value` of `total`, the arc in `ink`, the count inside. */
function Ring({ value, total, ink, label, testId }: { value: number; total: number; ink: string; label: string; testId: string }) {
  const r = 17
  const c = 2 * Math.PI * r
  const frac = total > 0 ? Math.max(0, Math.min(1, value / total)) : 0
  return (
    <div className="flex items-center gap-2.5" data-testid={testId} data-value={value} data-total={total}>
      <svg width="44" height="44" viewBox="0 0 44 44" role="img" aria-label={`${value} of ${total} ${label}`}>
        <circle cx="22" cy="22" r={r} fill="none" stroke="var(--hairline)" strokeWidth="5" />
        <circle
          cx="22" cy="22" r={r} fill="none" stroke={ink} strokeWidth="5" strokeLinecap="round"
          strokeDasharray={`${c * frac} ${c}`} transform="rotate(-90 22 22)"
        />
        <text x="22" y="26" textAnchor="middle" fontSize="11" fontWeight="700" fill="var(--silk)" className="tnum">
          {value}/{total}
        </text>
      </svg>
      <div className="text-[12px] leading-tight" style={{ color: 'var(--silk-dim)', maxWidth: 110 }}>{label}</div>
    </div>
  )
}

/** Serious and all findings as two counts, the serious one in the error ink. */
function FindingCounts({ serious, total }: { serious: number; total: number }) {
  return (
    <div className="flex items-center gap-3" data-testid="result-counts">
      <div className="text-center">
        <div className="text-[20px] font-bold tnum leading-none" style={{ color: serious > 0 ? 'var(--err)' : 'var(--silk-faint)' }}>{serious}</div>
        <div className="text-[11px] mt-1" style={{ color: 'var(--silk-dim)' }}>serious</div>
      </div>
      <div className="text-center">
        <div className="text-[20px] font-bold tnum leading-none" style={{ color: total > 0 ? 'var(--warn)' : 'var(--silk-faint)' }}>{total}</div>
        <div className="text-[11px] mt-1" style={{ color: 'var(--silk-dim)' }}>{total === 1 ? 'finding' : 'findings'}</div>
      </div>
    </div>
  )
}

const LEVEL_INK: Record<string, string> = { serious: 'var(--err)', warning: 'var(--warn)' }
const SHOWN_FINDINGS = 5

/** The findings that gate or warn, worst first, as rows that reach the board. */
function FindingRows({ groups, onLocate }: {
  groups: (FindingGroup & { section: string })[]
  onLocate?: (x: number, y: number, label: string) => void
}) {
  const { vocab } = useContext(ResultLinkContext)
  if (groups.length === 0) return null
  const shown = groups.slice(0, SHOWN_FINDINGS)
  return (
    <div className="text-[13px]" style={{ color: 'var(--silk)' }}>
      <div className="pb-1.5 text-[11px] font-bold tracking-widest uppercase" style={{ color: 'var(--silk-faint)' }}>
        What it found
      </div>
      <ul className="list-none p-0 m-0" data-testid="result-findings">
        {shown.map((g, i) => {
          const first = g.items[0]
          const located = g.items.find(item => item.x !== undefined && item.y !== undefined)
          const named = vocab ? entitiesIn(g.items.map(item => item.what).join(' '), vocab) : []
          const ink = LEVEL_INK[g.level] ?? 'var(--note)'
          return (
            <ResultRow
              key={i}
              testId="result-finding"
              accent={ink}
              glyph={g.level === 'serious' ? <CloseIcon size={12} /> : <WarningIcon size={12} />}
              actions={(
                <>
                  {located && onLocate && (
                    <RowAction testId="result-show-on-board" onClick={() => onLocate(located.x!, located.y!, first.what)}>
                      Show on board
                    </RowAction>
                  )}
                  <AddCheck entities={named} />
                </>
              )}
            >
              <span
                className="inline-block mr-1.5 px-1.5 rounded text-[10px] font-bold uppercase tracking-wider align-middle"
                style={{ color: ink, border: `1px solid ${ink}` }}
              >
                {g.level}
              </span>
              <span className="text-[11px] mr-1" style={{ color: 'var(--silk-faint)' }}>{g.section}</span>
              <div className="mt-0.5"><Linked text={first.what} /></div>
              {g.items.length > 1 && (
                <div className="text-[12px]" style={{ color: 'var(--silk-dim)' }}>and {plural(g.items.length - 1, 'more place')} like it</div>
              )}
              {g.fix && (
                <div className="mt-0.5 text-[12px]" style={{ color: 'var(--silk-dim)' }}>
                  <span className="font-semibold">Fix: </span><Linked text={g.fix} />
                </div>
              )}
            </ResultRow>
          )
        })}
      </ul>
      {groups.length > SHOWN_FINDINGS && (
        <div className="pt-1.5 text-[12px]" style={{ color: 'var(--silk-dim)' }}>
          {plural(groups.length - SHOWN_FINDINGS, 'more finding')} in the sections below.
        </div>
      )}
    </div>
  )
}

/** The "Show result on board" switch. */
function OverlaySwitch({ on, onToggle }: { on: boolean; onToggle: () => void }) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={on}
      data-testid="result-overlay-toggle"
      onClick={onToggle}
      className="hb-btn hb-press inline-flex items-center gap-2 px-2.5 text-[12px] whitespace-nowrap"
      style={{ height: 30, color: on ? 'var(--silk)' : undefined }}
    >
      <span
        aria-hidden="true"
        className="relative inline-block rounded-full"
        style={{ width: 26, height: 14, background: on ? 'var(--copper)' : 'var(--hairline)', transition: 'background 0.15s' }}
      >
        <span
          className="absolute rounded-full"
          style={{ top: 2, left: on ? 14 : 2, width: 10, height: 10, background: 'var(--surface)', transition: 'left 0.15s' }}
        />
      </span>
      Show result on board
    </button>
  )
}

export function ResultPanel({
  report: r, view: v, explanation, onLocate, overlay, exportSlot, command, onOpenChecks,
}: {
  report: WebReport
  view: ReportView
  explanation: VerdictExplanation | null
  onLocate?: (x: number, y: number, label: string) => void
  /** The board overlay switch; absent when there is no drawn board. */
  overlay?: { on: boolean; onToggle: () => void }
  /** The Export menu, folded into the action bar. */
  exportSlot: React.ReactNode
  /** The CLI command that reproduces the run. */
  command: string
  onOpenChecks?: () => void
}) {
  const tone = TONE[v.tone]
  const tag = verdictTag(r)
  const { title, rest } = splitHeadline(v.headline)
  const scope = v.partlyJudged
  const [bound, keyParts] = (r.bind?.critical_parts_bound ?? '').split('/').map(Number)
  const groups = v.sections
    .flatMap(s => s.groups.map(g => ({ ...g, section: s.title.replace(/\s*\(.*\)$/, '') })))
    .filter(g => g.level === 'serious' || g.level === 'warning')
    .sort((a, b) => (a.level === b.level ? 0 : a.level === 'serious' ? -1 : 1))
  const nothingFound = r.verdict === 'pass' && groups.length === 0
  const hasRows = !!scope || v.conditions.length > 0 || groups.length > 0 || nothingFound || !!explanation

  return (
    <section
      data-testid="report-verdict"
      data-verdict={r.verdict ?? 'unknown'}
      aria-label={`Board result: ${tag}`}
      className="rounded-xl overflow-hidden"
      style={{ border: `1px solid ${v.palette.border}`, background: 'var(--surface)', boxShadow: 'var(--shadow-card)' }}
    >
      <div className="px-4 pt-3.5 pb-3" style={{ background: v.palette.background, borderBottom: `1px solid ${v.palette.border}` }}>
        <div className="flex items-start gap-3">
          <span
            aria-hidden="true"
            className="shrink-0 inline-flex items-center justify-center rounded-full"
            style={{ width: 34, height: 34, marginTop: 2, color: tone.ink, border: `2px solid ${tone.ink}`, background: 'var(--surface)' }}
          >
            {tone.glyph}
          </span>
          <div className="min-w-0 flex-1">
            <div className="flex flex-wrap items-baseline justify-between gap-x-3 gap-y-0.5">
              <span data-testid="result-tag" className="text-[11px] font-bold tracking-widest uppercase" style={{ color: tone.ink }}>
                VERDICT: {tag}
              </span>
              <span className="text-xs tnum" data-testid="report-inventory" style={{ color: 'var(--silk-dim)' }}>
                {v.title} · {plural(r.num_components, 'part')} · {plural(r.num_nets, 'net')}
              </span>
            </div>
            <h2 data-testid="result-title" className="mt-0.5 text-[18px] font-semibold leading-snug" style={{ color: 'var(--silk)', overflowWrap: 'anywhere' }}>
              {title}
            </h2>
            {rest && (
              <p data-testid="result-subtitle" className="mt-0.5 text-[13px] leading-relaxed" style={{ color: 'var(--silk-dim)', overflowWrap: 'anywhere' }}>
                <Linked text={rest} />
              </p>
            )}
            {v.reason && !v.partlyJudged && (
              <div className="text-[13px] mt-1.5" data-testid="report-verdict-reason" title={v.reason} style={{ color: 'var(--silk)', overflowWrap: 'anywhere' }}>
                <Linked text={v.reasonBrief ?? v.reason} />
              </div>
            )}
          </div>
        </div>

        {/* Coverage: how much of the board the verdict speaks for. Each gauge is
            an engine number: the judged scope, the bind count, the findings. */}
        <div className="mt-3 flex flex-wrap items-center gap-x-6 gap-y-3" data-testid="result-gauges">
          {scope && scope.total > 0 && (
            <Ring testId="result-gauge-judged" value={scope.judged} total={scope.total} ink="var(--warn)" label="net checks judged" />
          )}
          {Number.isFinite(bound) && Number.isFinite(keyParts) && keyParts > 0 && (
            <Ring
              testId="result-gauge-modelled" value={bound} total={keyParts}
              ink={bound === keyParts ? 'var(--ok)' : 'var(--warn)'} label="key parts modelled"
            />
          )}
          {r.total > 0 ? <FindingCounts serious={r.serious} total={r.total} /> : (
            <div className="flex items-center gap-2 text-[12px]" data-testid="result-counts" style={{ color: 'var(--silk-dim)' }}>
              <span className="inline-flex items-center justify-center rounded-full" style={{ width: 22, height: 22, color: 'var(--ok)', border: '2px solid var(--ok)' }}>
                <CheckIcon size={12} />
              </span>
              no findings
            </div>
          )}
        </div>
      </div>

      {hasRows && (
        <div className="px-4 pt-2.5 pb-3 space-y-3">
          <PartlyJudged scope={v.partlyJudged} />
          <VerdictConditions conditions={v.conditions} />
          <FindingRows groups={groups} onLocate={onLocate} />
          {nothingFound && v.conditions.length === 0 && (
            <ul className="list-none p-0 m-0 text-[13px]" style={{ color: 'var(--silk)' }}>
              <ResultRow testId="result-nothing-found" accent="var(--ok)" glyph={<CheckIcon size={12} />}>
                Nothing the checks found blocks ordering.
              </ResultRow>
            </ul>
          )}
          {explanation && <ExplainVerdict explanation={explanation} />}
        </div>
      )}

      {/* What this result can become: the board overlay, a file, a command. */}
      <div
        data-testid="result-actions"
        className="px-4 py-2.5 flex flex-wrap items-center gap-2"
        style={{ borderTop: '1px solid var(--hairline)', background: 'var(--surface-2)' }}
      >
        {overlay && <OverlaySwitch on={overlay.on} onToggle={overlay.onToggle} />}
        {exportSlot}
        <span
          className="inline-flex items-center gap-1.5 rounded-lg px-2 text-[11px] min-w-0"
          data-testid="result-cli"
          title={command}
          style={{ height: 30, border: '1px solid var(--hairline)', background: 'var(--code-bg)', color: 'var(--silk-dim)', maxWidth: '100%' }}
        >
          <TerminalIcon size={12} />
          <code className="truncate" style={{ fontFamily: 'var(--font-mono)' }}>{command}</code>
          <CopyButton text={command} label="Copy CLI command" testId="result-copy-cli" small />
        </span>
        {onOpenChecks && (
          <button
            type="button"
            data-testid="result-open-checks"
            onClick={onOpenChecks}
            className="hb-btn hb-press inline-flex items-center gap-1.5 px-2.5 text-[12px] whitespace-nowrap"
            style={{ height: 30 }}
          >
            <ChecksIcon size={13} />
            Open in Checks
          </button>
        )}
      </div>
    </section>
  )
}
