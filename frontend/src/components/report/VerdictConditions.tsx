// The assumptions a pass rests on, listed under the verdict headline. The CLI
// prints the same sentences after "VERDICT: clean, conditionally:". A
// conditional pass is still a pass, so this is a quiet list in the note colour,
// never an error box; nothing renders for an unconditional report.
//
// Under the verdict sits "Explain this verdict": what the verdict means (the
// engine's glossary words), why this one, and what to do next, with the raw
// evidence one more click down.

import type { PartlyJudgedView, VerdictExplanation } from '../../lib/report-view'
import { useTermEntry } from '../ui'

export function VerdictConditions({ conditions }: { conditions: string[] }) {
  if (conditions.length === 0) return null
  return (
    <ul
      data-testid="report-verdict-conditions"
      aria-label={conditions.length === 1 ? 'Condition this pass rests on' : 'Conditions this pass rests on'}
      className="mt-2 space-y-1.5 text-[13px] leading-relaxed list-none p-0"
      style={{ color: 'var(--silk)' }}
    >
      {conditions.map(condition => (
        <li
          key={condition}
          data-testid="report-verdict-condition"
          className="pl-3"
          style={{ borderLeft: '3px solid var(--note-accent)', overflowWrap: 'anywhere' }}
        >
          {condition}
        </li>
      ))}
    </ul>
  )
}

const SHOWN_NETS = 6

/** "a, b, c and 2 more", with the whole list in the tooltip. */
function NetList({ nets }: { nets: string[] }) {
  const shown = nets.slice(0, SHOWN_NETS).join(', ')
  const more = nets.length - SHOWN_NETS
  return <span title={nets.join(', ')}>{more > 0 ? `${shown} and ${more} more` : shown}</span>
}

/** A PARTLY JUDGED run is not a pass, so it gets its own block in the warning
 *  colour (not the quiet note accent a conditional pass uses): the parts with
 *  no usable model, what that left unjudged, and what was judged. The CLI and
 *  `--json` carry the same scope as one sentence. */
export function PartlyJudged({ scope }: { scope: PartlyJudgedView | null }) {
  if (!scope) return null
  const { openParts, wiring, electrical, judged, total } = scope
  const many = openParts.length !== 1
  return (
    <div
      data-testid="report-partly-judged"
      role="note"
      className="mt-2 pl-3 text-[13px] leading-relaxed"
      style={{ color: 'var(--silk)', borderLeft: '3px solid var(--warn)', overflowWrap: 'anywhere' }}
    >
      <div className="font-semibold" data-testid="partly-judged-label" style={{ color: 'var(--warn)' }}>
        Not a pass: partly judged
      </div>
      {openParts.length > 0 && (
        <div data-testid="partly-judged-open">
          <span className="font-semibold" style={{ color: 'var(--silk-dim)' }}>No usable model: </span>
          {openParts.join(', ')}
        </div>
      )}
      {wiring.length > 0 && (
        <div data-testid="partly-judged-wiring">
          <span className="font-semibold" style={{ color: 'var(--silk-dim)' }}>Not judged, wiring and value checks on {wiring.length} {wiring.length === 1 ? 'net' : 'nets'}: </span>
          <NetList nets={wiring} />
        </div>
      )}
      {electrical.length > 0 && (
        <div data-testid="partly-judged-electrical">
          <span className="font-semibold" style={{ color: 'var(--silk-dim)' }}>Not judged, electrical checks (levels, ratings, rails) on {electrical.length} {electrical.length === 1 ? 'net' : 'nets'}: </span>
          <NetList nets={electrical} />
        </div>
      )}
      {total > 0 && (
        <div data-testid="partly-judged-judged">
          <span className="font-semibold" style={{ color: 'var(--silk-dim)' }}>Judged: </span>
          {judged} of {total} net checks
        </div>
      )}
      {openParts.length > 0 && (
        <div data-testid="partly-judged-next">
          <span className="font-semibold" style={{ color: 'var(--silk-dim)' }}>To judge the rest: </span>
          give {openParts.join(', ')} {many ? 'models' : 'a model'}.
        </div>
      )}
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
        {explanation.why && row('Why', 'explain-why', explanation.why)}
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
