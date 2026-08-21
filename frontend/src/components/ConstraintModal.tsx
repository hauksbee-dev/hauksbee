import { useMemo, useState } from 'react'
import { ConstraintEditor, constraintIssues, emptyConstraint, type ConstraintDraft } from './ConstraintEditor'

const LABELS: Record<string, string> = {
  voltage: 'A net must sit at a voltage',
  rail_window: 'A rail excursion must stay within bounds',
  toggle: 'A net must blink',
  'boot-coverage': 'Firmware must drive a net by a deadline',
  max_current: 'A part must stay under a current',
  max_temp: 'A part must stay cool',
}

/**
 * Small in-place editor opened from the 2D report map. It uses the same
 * ConstraintEditor and preflight as the full Checks view; saving queues one
 * exact assertion into that view's single underlying spec without navigating.
 */
export function ConstraintModal({
  initial,
  onSave,
  onClose,
  onOpenChecks,
}: {
  initial: Partial<ConstraintDraft> & Pick<ConstraintDraft, 'kind'>
  onSave: (draft: ConstraintDraft) => void
  onClose: () => void
  onOpenChecks: () => void
}) {
  const [draft, setDraft] = useState<ConstraintDraft>(() => ({
    ...emptyConstraint(initial.kind, initial.net ?? '', initial.ref ?? ''),
    ...initial,
  }))
  const [attempted, setAttempted] = useState(false)
  const issues = useMemo(() => attempted ? constraintIssues(draft) : [], [attempted, draft])
  const title = LABELS[draft.kind] ?? 'Add a constraint'

  return (
    <div
      data-testid="constraint-modal-backdrop"
      className="fixed inset-0 z-[80] flex items-center justify-center px-4 py-6"
      style={{ background: 'rgba(8, 10, 13, 0.72)' }}
      role="presentation"
      onMouseDown={event => { if (event.target === event.currentTarget) onClose() }}
    >
      <section
        data-testid="constraint-modal"
        role="dialog"
        aria-modal="true"
        aria-labelledby="constraint-modal-title"
        className="hb-card w-full max-w-2xl px-4 py-4"
        style={{ boxShadow: 'var(--shadow-pop)', background: 'var(--surface)' }}
      >
        <div className="flex items-start justify-between gap-3">
          <div>
            <div className="text-[10px] font-bold tracking-widest uppercase" style={{ color: 'var(--copper)' }}>
              Board constraint
            </div>
            <h2 id="constraint-modal-title" className="mt-1 text-[16px] font-semibold" style={{ color: 'var(--silk)' }}>
              {title}
            </h2>
            <p className="mt-1 text-[12px] leading-relaxed" style={{ color: 'var(--silk-dim)' }}>
              Edit the exact assertion before it is added to the shared Checks spec.
              Nothing runs until you choose Run in Checks.
            </p>
          </div>
          <button type="button" className="hb-btn hb-press px-2.5 py-1 text-[11px]" onClick={onClose}>
            Close
          </button>
        </div>

        <div className="mt-4 rounded-lg px-3 py-3" style={{ border: '1px solid var(--hairline)', background: 'var(--surface-2)' }}>
          <ConstraintEditor
            draft={draft}
            issues={issues}
            onChange={patch => setDraft(previous => ({ ...previous, ...patch }))}
          />
        </div>

        {issues.length > 0 && (
          <div data-testid="constraint-modal-validation" className="mt-2 rounded-lg px-3 py-2 text-[12px]" style={{ background: 'var(--err-bg)', border: '1px solid var(--err-border)', color: 'var(--err-strong)' }}>
            Fill the highlighted fields: {issues.map(issue => issue.message).join(' · ')}
          </div>
        )}

        <div className="mt-4 flex flex-wrap items-center gap-2">
          <button
            type="button"
            data-testid="constraint-modal-save"
            className="hb-btn-primary hb-press px-3.5 py-1.5 text-[12px]"
            onClick={() => {
              setAttempted(true)
              const next = constraintIssues(draft)
              if (next.length === 0) {
                onSave(draft)
                onClose()
              }
            }}
          >
            Add to Checks spec
          </button>
          <button
            type="button"
            data-testid="constraint-modal-open-checks"
            className="hb-btn hb-press px-3 py-1.5 text-[12px]"
            onClick={onOpenChecks}
          >
            Open full Checks
          </button>
          <span className="text-[11px]" style={{ color: 'var(--silk-faint)' }}>
            The report stays on this board view.
          </span>
        </div>
      </section>
    </div>
  )
}
