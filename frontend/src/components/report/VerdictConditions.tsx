// The rows under the board result's title that qualify it.
//
// A conditional pass lists the assumptions it rests on, one row per sentence,
// verbatim (the CLI prints the same sentences after "VERDICT: clean,
// conditionally:"). A conditional pass is still a pass, so these sit in the
// quiet note colour, never an error box; nothing renders for an unconditional
// report.
//
// A PARTLY JUDGED run is not a pass, so its rows carry the warning colour: the
// parts with no usable model (each with the way to give it one), what that left
// unjudged, and what to do about it.
//
// Every part and net in a row is a chip that reaches the board (see
// ResultLinks), and each row offers what can be done about it.
//
// Under the verdict sits "Explain this verdict": what the verdict means (the
// engine's glossary words), why this one, and what to do next, with the raw
// evidence one more click down.

import { useContext } from 'react'
import { unjudgedGroups, unmodelledTail } from '../../lib/report-view'
import type { PartlyJudgedView, UnmodelledParts, VerdictExplanation } from '../../lib/report-view'
import { entitiesIn } from '../../lib/result-links'
import { useTermEntry } from '../ui'
import { AddCheck, ChipList, Linked, netsOf, partsOf, ResultLinkContext, RowAction, ShowOnBoard } from './ResultLinks'

/** One result row: a glyph in the row's colour, the sentence, and its actions. */
export function ResultRow({ testId, accent, glyph, children, actions }: {
  testId: string
  accent: string
  glyph: React.ReactNode
  children: React.ReactNode
  actions?: React.ReactNode
}) {
  return (
    <li
      data-testid={testId}
      className="result-row flex items-start gap-2.5 py-2"
      style={{ borderTop: '1px solid var(--hairline)', overflowWrap: 'anywhere' }}
    >
      <span
        aria-hidden="true"
        className="shrink-0 inline-flex items-center justify-center rounded-md"
        style={{ width: 22, height: 22, marginTop: 1, color: accent, background: 'var(--surface-2)', border: `1px solid ${accent}` }}
      >
        {glyph}
      </span>
      <div className="min-w-0 flex-1 flex flex-wrap items-start gap-x-3 gap-y-1.5">
        <div className="min-w-0 flex-1 leading-relaxed" style={{ flexBasis: 260 }}>{children}</div>
        {actions && <div className="flex flex-wrap items-center gap-1.5">{actions}</div>}
      </div>
    </li>
  )
}

/** The note glyph: a small "i" in a circle. */
const NoteGlyph = () => (
  <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.4" strokeLinecap="round">
    <path d="M12 11v6" /><path d="M12 7.5h.01" />
  </svg>
)
const PartGlyph = () => (
  <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round">
    <rect x="5" y="5" width="14" height="14" rx="2" strokeDasharray="3 2.5" /><path d="M9 2v3M15 2v3M9 19v3M15 19v3" />
  </svg>
)
const NetGlyph = () => (
  <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round">
    <path d="M3 12h4l3-6 4 12 3-6h4" strokeDasharray="0" />
  </svg>
)

export function VerdictConditions({ conditions }: { conditions: string[] }) {
  const { vocab, variantRefs, onPickVariant } = useContext(ResultLinkContext)
  if (conditions.length === 0) return null
  return (
    <div className="text-[13px]" style={{ color: 'var(--silk)' }}>
    <div aria-hidden="true" className="pb-1.5 text-[11px] font-bold tracking-widest uppercase" style={{ color: 'var(--silk-faint)' }}>
      {conditions.length === 1 ? 'Rests on one condition' : `Rests on ${conditions.length} conditions`}
    </div>
    <ul
      data-testid="report-verdict-conditions"
      aria-label={conditions.length === 1 ? 'Condition this pass rests on' : 'Conditions this pass rests on'}
      className="list-none p-0 m-0"
    >
      {conditions.map(condition => {
        const named = vocab ? entitiesIn(condition, vocab) : []
        const parts = named.flatMap(e => (e.kind === 'part' ? [e.ref] : []))
        const pinnable = parts.filter(ref => variantRefs?.has(ref))
        return (
          <ResultRow
            key={condition}
            testId="report-verdict-condition"
            accent="var(--note)"
            glyph={<NoteGlyph />}
            actions={(
              <>
                {onPickVariant && pinnable.map(ref => (
                  <RowAction key={ref} testId={`result-pick-variant-${ref}`} primary onClick={() => onPickVariant(ref)}
                    title={`Pin ${ref}'s variant in the spec with [[override]] select`}>
                    Pin {ref}'s variant
                  </RowAction>
                ))}
                <AddCheck entities={named} />
              </>
            )}
          >
            <span style={{ borderLeft: '3px solid var(--note-accent)', paddingLeft: 8, display: 'block' }}>
              <Linked text={condition} />
            </span>
          </ResultRow>
        )
      })}
    </ul>
    </div>
  )
}

/** A PARTLY JUDGED run's scope as rows in the warning colour (not the quiet
 *  note accent a conditional pass uses): the parts with no usable model (the
 *  one measure, in its one sentence, stated here and nowhere else on the
 *  board) and what that left unjudged (one list, each net named once, grouped
 *  by the kind of check). The verdict word above says it is not a pass, the
 *  judged count is the coverage gauge's, and the way to judge the rest is the
 *  card's Next step, so none of those is said again here. */
export function PartlyJudged({ scope, unmodelled, onList }: {
  scope: PartlyJudgedView | null
  unmodelled?: UnmodelledParts
  /** Opens the full list of parts, where each can be given a model. */
  onList?: () => void
}) {
  if (!scope) return null
  const parts = unmodelled ?? { key: scope.openParts, other: [], all: scope.openParts }
  const unjudged = unjudgedGroups(scope)
  const unjudgedNets = unjudged.flatMap(g => g.nets)
  const warn = 'var(--warn)'
  if (parts.all.length === 0 && unjudged.length === 0) return null
  return (
    <div data-testid="report-partly-judged" role="note" className="text-[13px]" style={{ color: 'var(--silk)' }}>
      <ul className="list-none p-0 m-0">
        {parts.all.length > 0 && (
          <ResultRow
            testId="partly-judged-open"
            accent={warn}
            glyph={<PartGlyph />}
            actions={(
              <>
                <ShowOnBoard entities={partsOf(parts.all)} at={null} id="open-parts" label="the parts with no usable model" />
                {onList && parts.all.length > 1 && (
                  <RowAction testId="show-guessed-parts" onClick={onList}>List all {parts.all.length}</RowAction>
                )}
              </>
            )}
          >
            <span data-testid="open-parts-sentence">
              {parts.key.length > 0 && <><ChipList entities={partsOf(parts.key)} joined />{' '}</>}
              {unmodelledTail(parts)}.
            </span>
          </ResultRow>
        )}
        {unjudged.length > 0 && (
          <ResultRow testId="partly-judged-unjudged" accent={warn} glyph={<NetGlyph />} actions={<AddCheck entities={netsOf(unjudgedNets)} />}>
            <span className="font-semibold" style={{ color: 'var(--silk-dim)' }}>
              Not judged on {unjudgedNets.length} {unjudgedNets.length === 1 ? 'net' : 'nets'}
              {unjudged.length > 1 ? ', by kind' : ''}:
            </span>
            {unjudged.map(group => (
              <div key={group.kind} data-testid={`partly-judged-kind-${group.kind}`} className="mt-1">
                <span style={{ color: 'var(--silk-dim)' }}>{group.label}, {group.nets.length} {group.nets.length === 1 ? 'net' : 'nets'}: </span>
                <ChipList entities={netsOf(group.nets)} />
              </div>
            ))}
          </ResultRow>
        )}
      </ul>
    </div>
  )
}

/** "Explain this verdict": three lines a newcomer can act on, and the engine's
 *  full account behind a second click. Closed by default except where the
 *  caller says the reader needs it now (`open`). */
export function ExplainVerdict({ explanation, open }: { explanation: VerdictExplanation; open?: boolean }) {
  const entry = useTermEntry(explanation.term)
  const doThis = explanation.doThis ?? entry?.what_to_do ?? null
  if (!entry && !explanation.why && !doThis) return null
  const row = (label: string, testId: string, body: React.ReactNode) => (
    <div className="mt-1.5 first:mt-0" data-testid={testId}>
      <span className="font-semibold" style={{ color: 'var(--silk-dim)' }}>{label}: </span>
      {body}
    </div>
  )
  return (
    <details className="mt-2 text-[12px] leading-relaxed" data-testid="explain-verdict" open={open}>
      <summary className="cursor-pointer font-semibold" style={{ color: 'var(--copper-hi)' }}>
        Explain this verdict
      </summary>
      <div className="mt-1.5 pl-3" style={{ color: 'var(--silk)', borderLeft: '2px solid var(--hairline)' }}>
        {entry && row('What this means', 'explain-means', <><b style={{ fontWeight: 600 }}>{entry.label}.</b> {entry.short}</>)}
        {explanation.why && row('Why', 'explain-why', <Linked text={explanation.why} />)}
        {doThis && row('Do this', 'explain-do', doThis)}
        {explanation.evidence.length > 0 && (
          <details className="mt-2" data-testid="explain-evidence">
            <summary className="cursor-pointer" style={{ color: 'var(--silk-dim)' }}>Show the evidence</summary>
            {explanation.evidence.map(([label, value]) => (
              <div key={label} className="mt-1" style={{ overflowWrap: 'anywhere' }}>
                <b style={{ color: 'var(--silk-dim)', fontWeight: 600 }}>{label}:</b> {value}
              </div>
            ))}
          </details>
        )}
      </div>
    </details>
  )
}
