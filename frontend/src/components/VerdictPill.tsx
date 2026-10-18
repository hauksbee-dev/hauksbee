import { savedVerdict, TONE_STYLE, useStatusLabel } from '../lib/status-vocab'
import type { ChipTone, StatusTerm } from '../lib/status-vocab'

// A verdict as a small pill in the glossary's own words: the saved-session rows
// and the resume card show the same claim the header chip made when the run was
// last written.

export function VerdictPill({ verdict: stored, testId }: {
  verdict: { term: StatusTerm; detail: string; tone: ChipTone }
  testId?: string
}) {
  const label = useStatusLabel()
  const verdict = savedVerdict(stored)
  return (
    <span
      data-testid={testId}
      className="inline-flex items-center px-2 rounded-full text-[10px] font-semibold min-w-0 max-w-full"
      style={{ ...TONE_STYLE[verdict.tone], height: 18 }}
    >
      <span className="truncate">{label(verdict.term)}{verdict.detail ? ` · ${verdict.detail}` : ''}</span>
    </span>
  )
}
