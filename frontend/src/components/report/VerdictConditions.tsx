// The assumptions a pass rests on, listed under the verdict headline. The CLI
// prints the same sentences after "VERDICT: clean, conditionally:". A
// conditional pass is still a pass, so this is a quiet list in the note colour,
// never an error box; nothing renders for an unconditional report.
//
// Under the verdict sits "Explain this verdict": what the verdict means (the
// engine's glossary words), why this one, and what to do next, with the raw
// evidence one more click down.

import type { VerdictExplanation } from '../../lib/report-view'
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
