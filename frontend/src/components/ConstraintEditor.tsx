import { checkKind, rowIssues } from '../lib/check-spec'
import type { ConstraintDraft, RowIssue } from '../lib/check-spec'
import { Field } from './checks/pieces'

export type { ConstraintDraft } from '../lib/check-spec'
export { emptyConstraint } from '../lib/check-spec'

/** One validation problem, in the UI's own field names. */
export type ConstraintIssue = RowIssue

/** The shared preflight: the Checks builder, the board modal and the exported
 *  spec all judge a constraint by exactly these rules. */
export const constraintIssues = rowIssues

/**
 * The assertion controls, driven by the kind's schema in lib/check-spec. Both
 * the full Checks builder and the board selection modal render through here,
 * so the two surfaces cannot drift in labels, fields or validation.
 */
export function ConstraintEditor({
  draft,
  onChange,
  issues = [],
}: {
  draft: ConstraintDraft
  onChange: (patch: Partial<ConstraintDraft>) => void
  issues?: ConstraintIssue[]
}) {
  const meta = checkKind(draft.kind)
  // An either/or requirement highlights every input that could satisfy it
  // (min OR max, freq OR toggles).
  const bad = (field: string) => issues.some(issue => (issue.fields as string[]).includes(field))
  return (
    <div className="flex flex-wrap gap-x-4 gap-y-2" data-testid="constraint-editor">
      {meta?.subject === 'net' && (
        <Field
          label="net" value={draft.net} width={170} list="net-options" invalid={bad('net')}
          onChange={value => onChange({ net: value })}
        />
      )}
      {meta?.subject === 'ref' && (
        <Field
          label="part (ref)" value={draft.ref} width={90} placeholder="U1" invalid={bad('ref')}
          onChange={value => onChange({ ref: value })}
        />
      )}
      {meta?.fields.filter(f => !f.active || f.active(draft)).map(f => (
        f.options ? (
          <label
            key={f.key}
            className="inline-flex items-center gap-1.5 text-[12px] min-w-0 max-w-full"
            style={{ color: 'var(--silk-faint)' }}
          >
            {f.label}
            <select
              className="hb-input min-w-0"
              data-testid={f.testId ?? `constraint-${f.key}`}
              style={{ width: f.width, maxWidth: '100%' }}
              value={f.value ? f.value(draft) : draft[f.key]}
              onChange={event => {
                const value = event.currentTarget.value
                onChange({ ...f.clears?.(value), [f.key]: value } as Partial<ConstraintDraft>)
              }}
            >
              {f.options.map(option => (
                <option key={option.value} value={option.value}>{option.label}</option>
              ))}
            </select>
          </label>
        ) : (
          <Field
            key={f.key}
            label={f.label}
            value={draft[f.key]}
            width={f.width}
            placeholder={f.placeholder}
            invalid={bad(f.key)}
            onChange={value => onChange({ [f.key]: value } as Partial<ConstraintDraft>)}
          />
        )
      ))}
    </div>
  )
}
