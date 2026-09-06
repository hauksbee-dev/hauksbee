import { useMemo } from 'react'
import type { ArtifactProvenance, CheckResult, EvidenceAssumption } from '../../types/report'
import { assumptionsForEvidence, describeModelSource } from '../../lib/report-view'
import { summarizeErrorBudget } from '../../lib/report-view'
import { checkKind, tomlToBuilder } from '../../lib/check-spec'
import { VerdictBadge } from '../../motion'

// The small pieces the Checks view repeats: a labelled input, the PASS/FAIL
// chip that rides on a row, and the evidence block under it.

/** One labelled builder input. `width` is what the value wants; on a narrow
 *  column it is a ceiling and the field takes what is left rather than pushing
 *  the row off the card. */
export function Field({ label, value, onChange, width = 90, placeholder, invalid, list }: {
  label: string
  value: string
  onChange: (v: string) => void
  width?: number
  placeholder?: string
  /** Highlight as the offending input of a validation message. */
  invalid?: boolean
  /** Id of a datalist to complete from (the shared net picker). */
  list?: string
}) {
  return (
    <label
      className="inline-flex items-center gap-1.5 text-[12px] min-w-0 max-w-full"
      style={{ color: invalid ? 'var(--err)' : 'var(--silk-faint)' }}
    >
      {label}
      <input
        className={`hb-input tnum min-w-0${list ? ' flex-1' : ''}`}
        aria-invalid={invalid || undefined}
        list={list}
        style={{
          // With a picker the field grows into the row and `width` caps it;
          // without one `width` is what the value wants, capped by the column.
          ...(list ? { maxWidth: width } : { width, maxWidth: '100%' }),
          ...(invalid ? { borderColor: 'var(--err)', background: 'var(--err-bg)' } : {}),
        }}
        value={value}
        placeholder={placeholder}
        onChange={e => onChange(e.target.value)}
      />
    </label>
  )
}

/** Inline PASS / FAIL / INVALID chip riding on a check row after a run. */
export function ResultChip({ result, stale }: { result: CheckResult; stale: boolean }) {
  const tone = result.invalid
    ? { color: 'var(--warn)', bg: 'var(--warn-bg)', border: 'var(--warn-border)', label: 'INVALID' }
    : result.passed
      ? { color: 'var(--ok)', bg: 'var(--ok-bg)', border: 'var(--ok-border)', label: 'PASS' }
      : { color: 'var(--err)', bg: 'var(--err-bg)', border: 'var(--err-border)', label: 'FAIL' }
  // The word swaps (PASS becoming FAIL on a re-run) inside a fixed grid cell,
  // so the row never reflows on a verdict change. The colour is not animated:
  // it is applied by the wrapper's style, which means the chip is the right
  // colour on its first painted frame.
  return (
    <VerdictBadge
      data-testid="row-result"
      label={tone.label}
      title={stale ? `${result.detail} (from the last run; the spec has changed since)` : result.detail}
      className="px-2 rounded-full text-[10px] font-bold tracking-wide"
      style={{
        background: tone.bg, border: `1px solid ${tone.border}`, color: tone.color,
        height: 20, opacity: stale ? 0.45 : 1, flexShrink: 0,
      }}
    />
  )
}

/** Canonical assertion evidence from the CI runner. Wording is rendered
 * verbatim from the shared registry so web, CLI, JSON and CI artifacts cannot
 * drift into different explanations of the same limitation. */
export function AssertionEvidence({ result, assumptions, inventory, stale }: {
  result: CheckResult
  assumptions: readonly EvidenceAssumption[]
  inventory: readonly ArtifactProvenance[]
  stale: boolean
}) {
  const map = result.evidence
  if (!map) return null
  const linked = assumptionsForEvidence(map, assumptions)
  const artifacts = (map.artifacts ?? []).flatMap(index => inventory[index] ? [inventory[index]] : [])
  if (map.status === 'clean' && linked.length === 0 && !map.error_budget) return null
  return (
    <div
      data-testid="assertion-evidence"
      className="mt-2 rounded-md px-2.5 py-2 text-[11px]"
      style={{
        background: 'var(--surface-2)',
        border: '1px solid var(--hairline)',
        color: 'var(--silk-dim)',
        opacity: stale ? 0.55 : 1,
      }}
    >
      <div
        className="font-semibold uppercase tracking-wide"
        style={{ color: map.status === 'undermined' ? 'var(--warn)' : 'var(--silk-faint)' }}
      >
        Evidence {map.status}{map.error_budget ? ' · numeric qualification' : ''}
      </div>
      {map.error_budget && (
        <div className="mt-1.5" data-testid="error-budget-summary">
          {summarizeErrorBudget(map.error_budget).map(row => <div key={row}>{row}</div>)}
        </div>
      )}
      {linked.map(assumption => (
        <div key={assumption.id} className="mt-1.5">
          <code style={{ color: 'var(--copper)', fontFamily: 'var(--font-mono)' }}>{assumption.id}</code>
          <div style={{ color: 'var(--silk)' }}>{assumption.statement}</div>
          <div>{assumption.because} {assumption.consequence}</div>
          <div>To remove it: {assumption.replacement}</div>
        </div>
      ))}
      {artifacts.length > 0 && (
        <div className="mt-1.5" style={{ color: 'var(--silk-faint)' }}>
          Inputs: {artifacts.map(artifact => artifact.path.split('/').pop() ?? artifact.path).join(', ')}
        </div>
      )}
      {(map.models ?? []).length > 0 && (
        <div className="mt-1.5" data-testid="model-provenance" style={{ color: 'var(--silk-faint)' }}>
          Models: {(map.models ?? []).map(describeModelSource).join('; ')}
        </div>
      )}
    </div>
  )
}

/** The left column while raw-edit mode owns the spec: a live, read-only
 *  summary of what the raw TOML currently says, so the column is not a blank
 *  void. Falls back to a plain sentence when the spec uses vocabulary the
 *  builder cannot parse. */
export function RawModeSummary({ rawText }: { rawText: string }) {
  const parsed = useMemo(() => tomlToBuilder(rawText), [rawText])
  const line = (label: string, body: string) => (
    <div className="mt-2">
      <span style={{ color: 'var(--silk)' }}>{label}:</span> {body}
    </div>
  )
  return (
    <div
      className="hb-card px-4 py-4 text-[13px] leading-relaxed"
      style={{ color: 'var(--silk-dim)' }}
      data-testid="raw-summary"
    >
      <div className="text-[11px] font-bold tracking-widest uppercase mb-2" style={{ color: 'var(--silk-faint)' }}>
        What the raw spec says
      </div>
      {parsed ? (
        <>
          <div>
            <b style={{ color: 'var(--silk)', fontWeight: 600 }}>{parsed.name}</b>
            {' '}· run length {parsed.duration} ms
          </div>
          {parsed.supplies.length > 0
            && line('Power supplies', parsed.supplies.map(s => `${s.net} at ${s.volts} V`).join(', '))}
          {parsed.peripherals.length > 0
            && line('Interactions', parsed.peripherals.map(p => `${p.id} (${p.kind}) on ${p.net}`).join(', '))}
          {parsed.sensors.length > 0 && line(
            'Register-map devices',
            parsed.sensors.map(s => `${s.id}${s.controller ? ` on ${s.controller}` : ''}`).join(', '),
          )}
          <ul className="mt-2 pl-4" style={{ listStyleType: 'disc' }}>
            {parsed.checks.map((c, i) => {
              const meta = checkKind(c.kind)
              const subject = c.net ? ` on ${c.net}` : c.ref ? ` for ${c.ref}` : ''
              return (
                <li key={i} className="my-0.5">
                  {meta?.label ?? c.kind}{subject}
                  <code className="text-[10px] ml-1.5" style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}>
                    {c.kind}
                  </code>
                </li>
              )
            })}
            {parsed.checks.length === 0 && <li>no assertions yet</li>}
          </ul>
        </>
      ) : (
        <div>
          The spec uses vocabulary beyond the visual builder (tolerances, scenarios,
          overrides ...), so it cannot be summarized here.
        </div>
      )}
      <div className="mt-3 text-[12px]" style={{ color: 'var(--silk-faint)' }}>
        The raw TOML on the right is the source of truth now. The visual builder comes
        back when the spec fits its vocabulary (the “back to the builder” switch on the
        pane).
      </div>
    </div>
  )
}

/** A section card in the builder column: title, optional caption, and the
 *  controls that add rows to it. */
export function BuilderSection({ title, caption, actions, testId, children }: {
  title: string
  caption?: React.ReactNode
  actions?: React.ReactNode
  testId?: string
  children: React.ReactNode
}) {
  return (
    <section className="hb-card px-4 py-3.5 mb-4" data-testid={testId}>
      <div className="flex flex-wrap items-center justify-between gap-2 mb-2">
        <div>
          <h2 className="text-[11px] font-bold tracking-widest uppercase" style={{ margin: 0, color: 'var(--silk-faint)' }}>
            {title}
          </h2>
          {caption && (
            <div className="text-[11px] mt-1" style={{ color: 'var(--silk-faint)' }}>{caption}</div>
          )}
        </div>
        {actions && <div className="flex flex-wrap gap-1.5">{actions}</div>}
      </div>
      {children}
    </section>
  )
}

/** A row's `remove` affordance: the same quiet text button everywhere. */
export function RemoveButton({ onClick, label = 'remove', className = '' }: {
  onClick: () => void
  label?: string
  className?: string
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={`hb-press text-[11px] cursor-pointer ${className}`}
      style={{ color: 'var(--silk-faint)', background: 'none', border: 'none' }}
    >
      {label}
    </button>
  )
}

/** The copper "+ something" text button used inside a row. */
export function AddInlineButton({ onClick, children }: { onClick: () => void; children: React.ReactNode }) {
  return (
    <button
      type="button"
      onClick={onClick}
      className="hb-press text-[11px] cursor-pointer"
      style={{ color: 'var(--copper)', background: 'none', border: 'none' }}
    >
      {children}
    </button>
  )
}

/** The bordered well one builder row sits in; red-edged while it has issues. */
export function RowCard({ testId, invalid, children }: {
  testId: string
  invalid: boolean
  children: React.ReactNode
}) {
  return (
    <div
      className="rounded-lg px-3 py-3 mb-2"
      data-testid={testId}
      style={{ background: 'var(--surface-2)', border: `1px solid ${invalid ? 'var(--err)' : 'var(--hairline)'}` }}
    >
      {children}
    </div>
  )
}

export function RowIssues({ issues }: { issues: string[] }) {
  return (
    <>
      {issues.map(issue => (
        <div key={issue} className="text-[11px] mt-1" style={{ color: 'var(--err-strong)' }}>{issue}</div>
      ))}
    </>
  )
}
