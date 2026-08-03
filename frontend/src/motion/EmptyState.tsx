import { type ReactNode } from 'react'
import { ArriveOnce } from './Stagger'

// A designed empty state: a sentence that says what is missing and one action
// that fixes it.
//
// This is here because the interior.dev collection's `emptyLabel` prop taught
// the lesson negatively: a grid with no matching items renders the string "No
// results" centred in grey, which is the shape of every empty state that has
// ever left a user stuck. An empty region is a question the interface asked and
// then refused to answer. The two required props below are the answer: what is
// absent, and the one thing to do about it.
//
// Motion is one fade-in on arrival. An empty state that animates repeatedly is
// drawing attention to the absence of content, which is the opposite of the job.

export function EmptyState({ title, body, action, testId }: {
  /** What is not here, as a statement. Not "No data". */
  title: string
  /** Why it is not here, or what would put something here. */
  body: ReactNode
  /** The one action. Optional only when the absence genuinely has no remedy
   *  from this surface. */
  action?: ReactNode
  testId?: string
}) {
  return (
    <ArriveOnce
      className="rounded-xl px-5 py-6 text-center"
      style={{ border: '1px dashed var(--hairline)', background: 'var(--surface)' }}
    >
      <div data-testid={testId}>
        <div className="text-[13px] font-semibold" style={{ color: 'var(--silk)' }}>
          {title}
        </div>
        <div
          className="mt-1.5 text-[12px] leading-relaxed mx-auto"
          style={{ color: 'var(--silk-dim)', maxWidth: '26rem' }}
        >
          {body}
        </div>
        {action && <div className="mt-3.5 flex justify-center">{action}</div>}
      </div>
    </ArriveOnce>
  )
}
