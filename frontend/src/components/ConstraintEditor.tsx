import { checkKind, rowIssues } from '../lib/check-spec'
import type { ConstraintDraft, RowIssue } from '../lib/check-spec'
import { Field } from './checks/pieces'
import { splitFields, type CatalogKind, type CatalogField, type Draft } from '../lib/check-catalog'

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

const isNetField = (key: string) => key === 'net' || key.endsWith('_net') || key === 'along'

/** One catalogue field as a labelled input: the unit rides beside it, the source of a
 *  pre-filled value sits underneath, and a missing required value is marked. */
function CatalogInput({ field, value, note, invalid, onChange, nets }: {
  field: CatalogField
  value: string
  note?: string
  invalid: boolean
  onChange: (value: string) => void
  nets?: readonly string[]
}) {
  const id = `field-${field.key}`
  const choices = field.type === 'boolean' ? ['', 'true', 'false']
    : field.key === 'level' ? ['high', 'low'] : null
  return (
    <div className="flex min-w-0 flex-col gap-0.5" style={{ flex: '1 1 150px' }}>
      <label htmlFor={id} className="text-[11px]" style={{ color: invalid ? 'var(--err)' : 'var(--silk-faint)' }} title={field.help}>
        {field.label}{field.required ? ' *' : ''}{field.unit ? ` (${field.unit})` : ''}
      </label>
      {choices ? (
        <select
          id={id} data-testid={id} className="hb-input min-w-0" value={value}
          aria-required={field.required || undefined} aria-invalid={invalid || undefined}
          onChange={e => onChange(e.currentTarget.value)}
        >
          {choices.map(c => <option key={c} value={c}>{c === '' ? 'choose' : c}</option>)}
        </select>
      ) : (
        <input
          id={id} data-testid={id} className="hb-input tnum min-w-0" value={value}
          inputMode={field.type === 'number' || field.type === 'integer' ? 'decimal' : undefined}
          list={isNetField(field.key) && nets ? 'catalog-net-options' : undefined}
          aria-required={field.required || undefined} aria-invalid={invalid || undefined}
          style={invalid ? { borderColor: 'var(--err)', background: 'var(--err-bg)' } : undefined}
          onChange={e => onChange(e.currentTarget.value)}
        />
      )}
      {note && <span className="text-[10px]" style={{ color: 'var(--silk-faint)' }} data-testid={`${id}-note`}>{note}</span>}
    </div>
  )
}

/**
 * The editor for any catalogue kind: required fields and fields a source can fill
 * come first, every other option waits under "More options". Used by the selection
 * card, the "Add a check" menu and, through them, every one of the 19 kinds.
 */
export function CatalogFields({ entry, draft, notes = {}, invalid = [], onChange, nets }: {
  entry: CatalogKind
  draft: Draft
  notes?: Record<string, string>
  /** Keys of fields to mark, after an attempt to add. */
  invalid?: readonly string[]
  onChange: (key: string, value: string) => void
  nets?: readonly string[]
}) {
  const { main, more } = splitFields(entry)
  const render = (f: CatalogField) => (
    <CatalogInput
      key={f.key} field={f} value={draft[f.key] ?? ''} note={notes[f.key]}
      invalid={invalid.includes(f.key)} nets={nets} onChange={v => onChange(f.key, v)}
    />
  )
  return (
    <div className="flex flex-col gap-2" data-testid="catalog-fields">
      {nets && (
        <datalist id="catalog-net-options">{nets.map(n => <option key={n} value={n} />)}</datalist>
      )}
      <div className="flex flex-wrap gap-x-3 gap-y-2">{main.map(render)}</div>
      {more.length > 0 && (
        <details>
          <summary className="cursor-pointer text-[11px]" style={{ color: 'var(--silk-dim)' }}>More options</summary>
          <div className="mt-2 flex flex-wrap gap-x-3 gap-y-2">{more.map(render)}</div>
        </details>
      )}
    </div>
  )
}
