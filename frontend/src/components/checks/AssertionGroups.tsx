import { AnimatePresence, motion, useReducedMotion } from 'motion/react'
import type { ArtifactProvenance, CheckResult, EvidenceAssumption } from '../../types/report'
import { CHECK_KINDS, GROUP_ORDER, checkKind } from '../../lib/check-spec'
import type { CheckRow, RowIssue } from '../../lib/check-spec'
import { ARRIVE, LEAVE, StaggerItem, ValueSettle } from '../../motion'
import { PlusIcon } from '../Icons'
import { AssertionEvidence, Field, ResultChip } from './pieces'

/** The assertions, grouped by kind. The groups stagger in once on mount (a
 *  restored board's saved spec arriving), which is the only time this list is
 *  new to the reader. */
export function AssertionGroups({
  checks, validation, stale, resultForRow, assumptions, inventory, onAdd, onUpdate, onRemove,
}: {
  checks: CheckRow[]
  validation: Map<number, RowIssue[]>
  stale: boolean
  resultForRow: (id: number) => CheckResult | null
  assumptions: readonly EvidenceAssumption[]
  inventory: readonly ArtifactProvenance[]
  onAdd: (kind: string) => void
  onUpdate: (id: number, patch: Partial<CheckRow>) => void
  onRemove: (id: number) => void
}) {
  const reduced = useReducedMotion()
  const grouped = GROUP_ORDER
    .map(group => ({
      group,
      kinds: CHECK_KINDS.filter(k => k.group === group),
      rows: checks.filter(c => checkKind(c.kind)?.group === group),
    }))
    .filter(g => g.rows.length > 0)

  return (
    <>
      {grouped.map(({ group, kinds, rows }, gi) => (
        <StaggerItem key={group} index={gi}>
          <section className="hb-card px-4 py-3.5 mb-4" data-testid={`check-group-${group}`}>
            <div className="flex items-center justify-between mb-2">
              <h2
                className="text-[11px] font-bold tracking-widest uppercase inline-flex items-center gap-2"
                style={{ margin: 0, color: 'var(--silk-faint)' }}
              >
                {group}
                <span
                  className="tnum text-[10px] px-1.5 rounded-full"
                  style={{ background: 'var(--surface-2)', border: '1px solid var(--hairline)', color: 'var(--silk-dim)' }}
                >
                  {rows.length}
                </span>
              </h2>
              {/* Add another of this group's first kind (the common case); the
                  global menu below covers everything. */}
              <button
                type="button"
                onClick={() => onAdd(kinds[0].kind)}
                className="hb-press text-[11px] cursor-pointer inline-flex items-center gap-1"
                style={{ color: 'var(--copper)', background: 'none', border: 'none' }}
              >
                <PlusIcon size={11} /> add
              </button>
            </div>

            {/* Rows appear and disappear as the reader composes the spec.
                Without presence, a removed row vanishes between frames and the
                rows below jump up into the gap, which reads as "something else
                also changed". The exit is deliberately faster than the entry
                (see ../../motion tokens): a row on its way out must not hold
                its slot open while the reader waits to see the result. */}
            <AnimatePresence initial={false}>
              {rows.map(c => {
                const meta = checkKind(c.kind)
                const rowResult = resultForRow(c.id)
                const issues = validation.get(c.id) ?? []
                // An either/or requirement highlights every input that could
                // satisfy it (min OR max, freq OR toggles).
                const bad = (field: keyof CheckRow) => issues.some(i => (i.fields as string[]).includes(field))
                return (
                  <motion.div
                    key={c.id}
                    layout={reduced ? false : 'position'}
                    initial={reduced ? false : { opacity: 0, y: -4 }}
                    animate={{ opacity: 1, y: 0 }}
                    exit={reduced ? { opacity: 0 } : { opacity: 0, height: 0, transition: LEAVE }}
                    transition={reduced ? { duration: 0 } : ARRIVE}
                    className="check-row py-2.5"
                    style={{ overflow: 'hidden' }}
                  >
                    <div className="flex items-center justify-between gap-2">
                      <div className="text-[13px] min-w-0 flex flex-wrap items-center gap-x-2" style={{ color: 'var(--silk)' }}>
                        {/* The plain-language label is what the row IS, so it
                            wraps rather than truncating: on a phone column the
                            kind chip and the verdict leave it about 24px. */}
                        <span>{meta?.label ?? c.kind}</span>
                        <code className="text-[10px] shrink-0" style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}>
                          {c.kind}
                        </code>
                        {rowResult && <ResultChip result={rowResult} stale={stale} />}
                      </div>
                      <button
                        type="button"
                        className="hb-press text-[12px] cursor-pointer shrink-0"
                        style={{ color: 'var(--silk-faint)', background: 'none', border: 'none' }}
                        onClick={() => onRemove(c.id)}
                      >
                        remove
                      </button>
                    </div>
                    <div className="mt-2 flex flex-wrap gap-x-4 gap-y-2">
                      {meta?.subject === 'net' && (
                        <Field
                          label="net" value={c.net} width={170} list="net-options" invalid={bad('net')}
                          onChange={v => onUpdate(c.id, { net: v })}
                        />
                      )}
                      {meta?.subject === 'ref' && (
                        <Field
                          label="part (ref)" value={c.ref} width={90} placeholder="U1"
                          invalid={bad('ref')} onChange={v => onUpdate(c.id, { ref: v })}
                        />
                      )}
                      {meta?.fields.map(f => (
                        <Field
                          key={f.key}
                          label={f.label}
                          value={c[f.key]}
                          width={f.width}
                          placeholder={f.placeholder}
                          invalid={bad(f.key)}
                          onChange={v => onUpdate(c.id, { [f.key]: v })}
                        />
                      ))}
                    </div>
                    {/* The missing-values verdict, on the row it judges, in the
                        builder's own field names. */}
                    {issues.length > 0 && (
                      <div data-testid="row-validation" className="mt-1.5 text-[11px]" style={{ color: 'var(--err)' }}>
                        Not run: {issues.map(i => i.message).join(' · ')}
                      </div>
                    )}
                    {rowResult && rowResult.detail && (
                      <div
                        data-testid="row-result-detail"
                        className="mt-1.5 text-[11px] tnum"
                        style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)', opacity: stale ? 0.55 : 1 }}
                      >
                        {/* The measured number rolls to its new value on a
                            re-run: up from below when it rose, down from above
                            when it fell. That direction is the only thing the
                            animation carries. No colour: a measurement going up
                            is not good news, it is a measurement. */}
                        <ValueSettle value={rowResult.detail} />
                        {stale ? ' (from the last run; the spec has changed since)' : ''}
                      </div>
                    )}
                    {rowResult && (
                      <AssertionEvidence
                        result={rowResult}
                        assumptions={assumptions}
                        inventory={inventory}
                        stale={stale}
                      />
                    )}
                  </motion.div>
                )
              })}
            </AnimatePresence>
          </section>
        </StaggerItem>
      ))}
    </>
  )
}

/** True when no check kind in the spec belongs to a rendered group. */
export const hasNoGroups = (checks: CheckRow[]): boolean => !checks.some(c => checkKind(c.kind))
