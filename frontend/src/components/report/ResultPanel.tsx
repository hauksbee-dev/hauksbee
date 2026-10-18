// The board result card: the one verdict display on the Board view.
//
//   ┌ [glyph] VERDICT WORD  detail                        board · parts · nets ┐
//   │ What the run found, in one sentence (the headline, verdict words cut)   │
//   │ (ring) net checks judged  (ring) key parts modelled                     │
//   ├ rows: what the pass rests on, or what was left unjudged ────────────────┤
//   ├ Next: the one next step for this board, with its button ────────────────┤
//   └ [Export ▾] [Open in Checks] ─────────────────────────────────────────────┘
//
// The verdict word and its colour are status-vocab's (the word every surface
// uses); the sentence and rows are the engine's. The findings themselves sit
// beside the board map, not here. Nothing here re-judges the board.

import type { WebReport } from '../../types/report'
import type { ReportView, VerdictExplanation } from '../../lib/report-view'
import { plural, reasonForCard, verdictSentence } from '../../lib/report-view'
import { ringLabel } from '../../lib/gauge'
import { boardVerdict, TONE_STYLE, useStatusLabel } from '../../lib/status-vocab'
import type { ChipTone } from '../../lib/status-vocab'
import { CheckIcon, ChecksIcon, CloseIcon, WarningIcon } from '../Icons'
import { ExplainVerdict, PartlyJudged, VerdictConditions } from './VerdictConditions'
import { Linked } from './ResultLinks'

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

const GLYPH: Record<ChipTone, React.ReactNode> = {
  ok: <CheckIcon size={15} />,
  warn: <WarningIcon size={15} />,
  err: <CloseIcon size={15} />,
  quiet: <WarningIcon size={15} />,
}

/** A ring gauge: `value` of `total`, the arc in `ink`, the count inside. A count
 *  too long for the hole is a percentage inside and the exact numbers beside
 *  (see `ringLabel`), so no count is ever clipped to a wrong-looking number. */
function Ring({ value, total, ink, label, testId }: { value: number; total: number; ink: string; label: string; testId: string }) {
  const r = 15
  const c = 2 * Math.PI * r
  const frac = total > 0 ? Math.max(0, Math.min(1, value / total)) : 0
  const { center, exact } = ringLabel(value, total)
  const full = `${value} of ${total} ${label}`
  return (
    <div className="flex items-center gap-2 min-w-0" data-testid={testId} data-value={value} data-total={total} title={full}>
      <svg width="38" height="38" viewBox="0 0 38 38" role="img" aria-label={full} className="shrink-0">
        <circle cx="19" cy="19" r={r} fill="none" stroke="var(--hairline)" strokeWidth="4" />
        <circle
          cx="19" cy="19" r={r} fill="none" stroke={ink} strokeWidth="4" strokeLinecap="round"
          strokeDasharray={`${c * frac} ${c}`} transform="rotate(-90 19 19)"
        />
        <text
          data-testid={`${testId}-text`}
          x="19" y="22.5" textAnchor="middle" fontSize={exact ? 8.5 : 10} fontWeight="700" fill="var(--silk)" className="tnum"
        >
          {center}
        </text>
      </svg>
      <div className="text-[12px] leading-tight min-w-0" style={{ color: 'var(--silk-dim)', maxWidth: 130 }}>
        {exact && <div className="tnum font-semibold" data-testid={`${testId}-exact`} style={{ color: 'var(--silk)' }}>{exact}</div>}
        {label}
      </div>
    </div>
  )
}

export function ResultPanel({
  report: r, view: v, explanation, exportSlot, onOpenChecks, next,
}: {
  report: WebReport
  view: ReportView
  explanation: VerdictExplanation | null
  /** The Export menu, in the action bar. */
  exportSlot: React.ReactNode
  onOpenChecks?: () => void
  /** The card's one Next step (see NextStep); absent for a restored session. */
  next?: React.ReactNode
}) {
  const label = useStatusLabel()
  const verdict = boardVerdict(r)
  const tone: ChipTone = verdict?.tone ?? 'quiet'
  const style = TONE_STYLE[tone]
  const ink = String(style.color)
  const word = verdict ? label(verdict.term) : label('not_checked')
  const sentence = verdictSentence(v.headline)
  // The engine's reason, unless the sentence above already says it.
  const reason = v.reason && !v.partlyJudged ? reasonForCard(v.reason) : null
  const showReason = !!reason && !sentence.toLowerCase().includes(reason.toLowerCase().replace(/\.$/, ''))
  const scope = v.partlyJudged
  const [bound, keyParts] = (r.bind?.critical_parts_bound ?? '').split('/').map(Number)
  const modelled = Number.isFinite(bound) && Number.isFinite(keyParts) && keyParts > 0
  const judged = !!scope && scope.total > 0
  const hasRows = !!scope || v.conditions.length > 0 || !!explanation

  return (
    <section
      data-testid="report-verdict"
      data-verdict={r.verdict ?? 'unknown'}
      data-tone={tone}
      aria-label={`Board result: ${word}${verdict?.detail ? `, ${verdict.detail}` : ''}`}
      className="rounded-xl"
      style={{ border: style.border, background: 'var(--surface)', boxShadow: 'var(--shadow-card)' }}
    >
      <div className="px-4 pt-3 pb-3 rounded-t-xl" style={{ background: String(style.background) }}>
        <div className="flex items-start gap-3">
          <span
            aria-hidden="true"
            className="shrink-0 inline-flex items-center justify-center rounded-full"
            style={{ width: 30, height: 30, marginTop: 1, color: ink, border: `2px solid ${ink}`, background: 'var(--surface)' }}
          >
            {GLYPH[tone]}
          </span>
          <div className="min-w-0 flex-1">
            <div className="flex flex-wrap items-baseline justify-between gap-x-3 gap-y-0.5">
              <div className="flex flex-wrap items-baseline gap-x-2 min-w-0">
                <span data-testid="result-verdict-word" className="text-[18px] font-bold leading-tight" style={{ color: ink }}>
                  {word}
                </span>
                {verdict?.detail && (
                  <span data-testid="result-verdict-detail" className="text-[13px] font-semibold" style={{ color: ink }}>
                    {verdict.detail}
                  </span>
                )}
              </div>
              <span className="text-xs tnum" data-testid="report-inventory" style={{ color: 'var(--silk-dim)' }}>
                {v.title} · {plural(r.num_components, 'part')} · {plural(r.num_nets, 'net')}
              </span>
            </div>
            {sentence && (
              <h2 data-testid="result-title" className="mt-1 text-[14px] font-medium leading-snug" style={{ color: 'var(--silk)', overflowWrap: 'anywhere' }}>
                <Linked text={sentence} />
              </h2>
            )}
            {showReason && (
              <div className="text-[13px] mt-1" data-testid="report-verdict-reason" title={v.reason ?? undefined} style={{ color: 'var(--silk-dim)', overflowWrap: 'anywhere' }}>
                <Linked text={reason} />
              </div>
            )}
            {(judged || modelled) && (
              <div className="mt-2 flex flex-wrap items-center gap-x-5 gap-y-2" data-testid="result-gauges">
                {judged && (
                  <Ring testId="result-gauge-judged" value={scope.judged} total={scope.total} ink="var(--warn)" label="net checks judged" />
                )}
                {modelled && (
                  <Ring
                    testId="result-gauge-modelled" value={bound} total={keyParts}
                    ink={bound === keyParts ? 'var(--ok)' : 'var(--warn)'} label="key parts modelled"
                  />
                )}
              </div>
            )}
          </div>
        </div>
      </div>

      {hasRows && (
        <div className="px-4 pt-1 pb-2.5 space-y-2" style={{ borderTop: '1px solid var(--hairline)' }}>
          <PartlyJudged scope={v.partlyJudged} />
          <VerdictConditions conditions={v.conditions} />
          {explanation && <ExplainVerdict explanation={explanation} />}
        </div>
      )}

      {next && (
        <div className="px-4 py-2.5" style={{ borderTop: '1px solid var(--hairline)' }}>{next}</div>
      )}

      <div
        data-testid="result-actions"
        className="px-4 py-2 flex flex-wrap items-center gap-2 rounded-b-xl"
        style={{ borderTop: '1px solid var(--hairline)', background: 'var(--surface-2)' }}
      >
        {exportSlot}
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
