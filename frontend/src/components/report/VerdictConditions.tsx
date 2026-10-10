// The assumptions a pass rests on, listed under the verdict headline. The CLI
// prints the same sentences after "VERDICT: clean, conditionally:". A
// conditional pass is still a pass, so this is a quiet list in the note colour,
// never an error box; nothing renders for an unconditional report.

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
