import type { CSSProperties } from 'react'

/** The editable fields shared by a Checks row and the board-side modal. */
export interface ConstraintDraft {
  kind: string
  net: string
  ref: string
  min: string
  max: string
  after_ms: string
  deadline_ms: string
  contains: string
  freq_hz: string
  tolerance: string
  min_toggles: string
  amps: string
  celsius: string
  /** Rail-window polarity. Older saved rows omit this and default to dip. */
  rail_polarity: 'dip' | 'spike'
  dip_below: string
  for_max_ms: string
  recover_to: string
  recover_within_ms: string
  spike_above: string
  spike_for_max_ms: string
  settle_to: string
  settle_within_ms: string
}

export interface ConstraintIssue {
  field: keyof ConstraintDraft
  message: string
}

const NET_KINDS = ['voltage', 'toggle', 'boot-coverage', 'rail_window']
const REF_KINDS = ['max_current', 'max_temp']

const finite = (value: string) => value.trim() !== '' && Number.isFinite(Number(value.trim()))

function railPolarity(c: Partial<Pick<ConstraintDraft, 'rail_polarity' | 'spike_above'>>): 'dip' | 'spike' {
  // Rows saved before the polarity selector was introduced have no mode. A
  // populated over-voltage field is unambiguously a spike; otherwise retain
  // the original dip behaviour.
  return c.rail_polarity ?? ((c.spike_above ?? '').trim() ? 'spike' : 'dip')
}

/** The same builder preflight used by the Checks view and the modal. */
export function constraintIssues(c: ConstraintDraft): ConstraintIssue[] {
  // Saved rows created by an older frontend may not carry fields introduced
  // later. Treat a missing field exactly like an empty input; a migration in
  // ChecksView fills the row, and this guard keeps rendering fail-closed even
  // if a partial draft reaches the shared modal/editor directly.
  const blank = (value: string | undefined) => (value ?? '').trim() === ''
  const issues: ConstraintIssue[] = []
  const needNet = () => { if (blank(c.net)) issues.push({ field: 'net', message: 'net is empty' }) }
  switch (c.kind) {
    case 'voltage':
      needNet()
      if (blank(c.min) && blank(c.max)) issues.push({ field: 'min', message: 'needs a min V and/or a max V' })
      break
    case 'uart':
      if (blank(c.contains)) issues.push({ field: 'contains', message: '"must print" is empty' })
      break
    case 'toggle':
      needNet()
      if (blank(c.freq_hz) && blank(c.min_toggles)) issues.push({ field: 'freq_hz', message: 'needs a freq Hz or a min toggles' })
      break
    case 'boot-coverage':
      needNet()
      if (blank(c.min)) issues.push({ field: 'min', message: 'reach V is empty' })
      if (blank(c.deadline_ms)) issues.push({ field: 'deadline_ms', message: 'within ms is empty' })
      break
    case 'max_current':
      if (blank(c.ref)) issues.push({ field: 'ref', message: 'part (ref) is empty' })
      if (blank(c.amps)) issues.push({ field: 'amps', message: 'max A is empty' })
      break
    case 'max_temp':
      if (blank(c.ref)) issues.push({ field: 'ref', message: 'part (ref) is empty' })
      break
    case 'rail_window':
      needNet()
      if (railPolarity(c) === 'spike') {
        if (blank(c.spike_above)) {
          issues.push({ field: 'spike_above', message: 'spike above V is empty' })
        } else if (blank(c.spike_for_max_ms) && blank(c.settle_within_ms)) {
          issues.push({ field: 'spike_for_max_ms', message: 'needs a for max ms or a settling window (within ms)' })
        }
        if (!blank(c.settle_within_ms) && blank(c.settle_to)) {
          issues.push({ field: 'settle_to', message: 'settle to V is empty (needed with within ms)' })
        }
        if (!blank(c.settle_to) && blank(c.settle_within_ms)) {
          issues.push({ field: 'settle_within_ms', message: 'within ms is empty (needed with settle to V)' })
        }
      } else {
        if (blank(c.dip_below)) {
          issues.push({ field: 'dip_below', message: 'dip below V is empty' })
        } else if (blank(c.for_max_ms) && blank(c.recover_within_ms)) {
          issues.push({ field: 'for_max_ms', message: 'needs a for max ms or a recovery window (within ms)' })
        }
        if (!blank(c.recover_within_ms) && blank(c.recover_to)) {
          issues.push({ field: 'recover_to', message: 'recover to V is empty (needed with within ms)' })
        }
        if (!blank(c.recover_to) && blank(c.recover_within_ms)) {
          issues.push({ field: 'recover_within_ms', message: 'within ms is empty (needed with recover to V)' })
        }
      }
      break
    default:
      break
  }
  return issues
}

function Field({ label, value, onChange, width = 74, placeholder, invalid }: {
  label: string
  value: string
  onChange: (value: string) => void
  width?: number
  placeholder?: string
  invalid?: boolean
}) {
  const style: CSSProperties = {
    width,
    maxWidth: '100%',
    boxSizing: 'border-box',
    ...(invalid ? { borderColor: 'var(--err)', background: 'var(--err-bg)' } : {}),
  }
  return (
    <label className="flex w-full flex-wrap items-center gap-1.5 text-[12px] min-w-0 max-w-full sm:w-auto" style={{ color: invalid ? 'var(--err)' : 'var(--silk-faint)' }}>
      {label}
      <input
        className="hb-input tnum min-w-0"
        style={style}
        value={value}
        placeholder={placeholder}
        aria-invalid={invalid || undefined}
        onChange={event => onChange(event.currentTarget.value)}
      />
    </label>
  )
}

/**
 * The assertion controls used in both the full Checks builder and the board
 * selection modal. Keeping this one renderer prevents the two surfaces from
 * drifting in labels, fields, or validation semantics.
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
  const bad = (field: keyof ConstraintDraft) => issues.some(issue => issue.field === field)
  return (
    <div className="flex flex-wrap gap-x-4 gap-y-2" data-testid="constraint-editor">
      {NET_KINDS.includes(draft.kind) && (
        <label className="flex w-full flex-wrap items-center gap-1.5 text-[12px] min-w-0 max-w-full sm:w-auto" style={{ color: bad('net') ? 'var(--err)' : 'var(--silk-faint)' }}>
          net
          <input
            className="hb-input min-w-0"
            style={{ width: 190, maxWidth: 'calc(100% - 12px)', boxSizing: 'border-box', ...(bad('net') ? { borderColor: 'var(--err)', background: 'var(--err-bg)' } : {}) }}
            list="net-options"
            value={draft.net}
            aria-invalid={bad('net') || undefined}
            onChange={event => onChange({ net: event.currentTarget.value })}
          />
        </label>
      )}
      {REF_KINDS.includes(draft.kind) && (
        <Field label="part (ref)" value={draft.ref} width={90} placeholder="U1" invalid={bad('ref')} onChange={value => onChange({ ref: value })} />
      )}
      {draft.kind === 'voltage' && (
        <>
          <Field label="min V" value={draft.min} width={64} invalid={bad('min')} onChange={value => onChange({ min: value })} />
          <Field label="max V" value={draft.max} width={64} invalid={bad('min')} onChange={value => onChange({ max: value })} />
          <Field label="after ms" value={draft.after_ms} width={64} onChange={value => onChange({ after_ms: value })} />
        </>
      )}
      {draft.kind === 'uart' && (
        <Field label="must print" value={draft.contains} width={220} placeholder="hello" invalid={bad('contains')} onChange={value => onChange({ contains: value })} />
      )}
      {draft.kind === 'toggle' && (
        <>
          <Field label="freq Hz" value={draft.freq_hz} width={64} invalid={bad('freq_hz')} onChange={value => onChange({ freq_hz: value })} />
          <Field label="±tol" value={draft.tolerance} width={56} onChange={value => onChange({ tolerance: value })} />
          <Field label="or min toggles" value={draft.min_toggles} width={64} invalid={bad('freq_hz')} onChange={value => onChange({ min_toggles: value })} />
        </>
      )}
      {draft.kind === 'boot-coverage' && (
        <>
          <Field label="reach V" value={draft.min} width={64} invalid={bad('min')} onChange={value => onChange({ min: value })} />
          <Field label="within ms" value={draft.deadline_ms} width={64} invalid={bad('deadline_ms')} onChange={value => onChange({ deadline_ms: value })} />
        </>
      )}
      {draft.kind === 'max_current' && (
        <Field label="max A" value={draft.amps} width={64} invalid={bad('amps')} onChange={value => onChange({ amps: value })} />
      )}
      {draft.kind === 'max_temp' && (
        <Field label="max °C (blank = part rating)" value={draft.celsius} width={70} onChange={value => onChange({ celsius: value })} />
      )}
      {draft.kind === 'rail_window' && (
        <>
          <label className="inline-flex items-center gap-1.5 text-[12px] min-w-0 max-w-full" style={{ color: 'var(--silk-faint)' }}>
            excursion
            <select
              className="hb-input min-w-0"
              data-testid="rail-polarity"
              value={railPolarity(draft)}
              onChange={event => {
                const polarity = event.currentTarget.value as 'dip' | 'spike'
                onChange(polarity === 'spike'
                  ? { rail_polarity: polarity, dip_below: '', for_max_ms: '', recover_to: '', recover_within_ms: '' }
                  : { rail_polarity: polarity, spike_above: '', spike_for_max_ms: '', settle_to: '', settle_within_ms: '' })
              }}
            >
              <option value="dip">dip below</option>
              <option value="spike">spike above</option>
            </select>
          </label>
          {railPolarity(draft) === 'spike' ? (
            <>
              <Field label="spike above V" value={draft.spike_above} width={78} invalid={bad('spike_above')} onChange={value => onChange({ spike_above: value })} />
              <Field label="for max ms" value={draft.spike_for_max_ms} width={64} invalid={bad('spike_for_max_ms')} onChange={value => onChange({ spike_for_max_ms: value })} />
              <Field label="settle to V" value={draft.settle_to} width={64} invalid={bad('settle_to')} onChange={value => onChange({ settle_to: value })} />
              <Field label="within ms" value={draft.settle_within_ms} width={64} invalid={bad('settle_within_ms')} onChange={value => onChange({ settle_within_ms: value })} />
            </>
          ) : (
            <>
              <Field label="dip below V" value={draft.dip_below} width={64} invalid={bad('dip_below')} onChange={value => onChange({ dip_below: value })} />
              <Field label="for max ms" value={draft.for_max_ms} width={64} invalid={bad('for_max_ms')} onChange={value => onChange({ for_max_ms: value })} />
              <Field label="recover to V" value={draft.recover_to} width={64} invalid={bad('recover_to')} onChange={value => onChange({ recover_to: value })} />
              <Field label="within ms" value={draft.recover_within_ms} width={64} invalid={bad('recover_within_ms')} onChange={value => onChange({ recover_within_ms: value })} />
            </>
          )}
        </>
      )}
    </div>
  )
}

export function emptyConstraint(kind: string, net = '', ref = ''): ConstraintDraft {
  return {
    kind, net, ref, min: '', max: '', after_ms: '', deadline_ms: '', contains: '',
    freq_hz: '', tolerance: '', min_toggles: '', amps: '', celsius: '', rail_polarity: 'dip', dip_below: '',
    for_max_ms: '', recover_to: '', recover_within_ms: '', spike_above: '', spike_for_max_ms: '', settle_to: '', settle_within_ms: '',
  }
}

/** Keep this helper available to callers that need the numeric-field hint. */
export const isFiniteConstraintNumber = finite
