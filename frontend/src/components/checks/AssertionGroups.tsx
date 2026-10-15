import { useState } from 'react'
import { AnimatePresence, motion, useReducedMotion } from 'motion/react'
import type { ArtifactProvenance, CheckResult, EvidenceAssumption } from '../../types/report'
import { checkKind } from '../../lib/check-spec'
import { groupOf, KIND_GROUPS, useCatalog } from '../../lib/check-catalog'
import { repeatedAssumptions } from '../../lib/report-view'
import type { CheckRow, RowIssue } from '../../lib/check-spec'
import { ARRIVE, LEAVE, StaggerItem } from '../../motion'
import { PlusIcon } from '../Icons'
import { AssertionEvidence, ResultChip } from './pieces'
import { ConstraintEditor } from '../ConstraintEditor'

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
  // A row's evidence is a wall of text; it opens on request, never by default.
  const [evidenceOpen, setEvidenceOpen] = useState<ReadonlySet<number>>(new Set())
  const toggleEvidence = (id: number) => setEvidenceOpen(prev => {
    const next = new Set(prev)
    if (!next.delete(id)) next.add(id)
    return next
  })
  // The groups, the order and the plain-language titles are the served
  // catalogue's (lib/check-catalog); the builder only says which kinds it has a row for.
  const catalog = useCatalog()
  const titleOf = (kind: string) => catalog?.find(k => k.kind === kind)?.title ?? kind
  const grouped = KIND_GROUPS
    .map(([group, members]) => ({
      group,
      kinds: members.filter(kind => checkKind(kind)),
      rows: checks.filter(c => groupOf(checkKind(c.kind)?.kind ?? c.kind) === group),
    }))
    .filter(g => g.rows.length > 0)
  const displayRows = grouped.flatMap(group => group.rows)
  const repeats = repeatedAssumptions(displayRows.map(row => resultForRow(row.id)))

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
                onClick={() => onAdd(kinds[0])}
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
                const rowResult = resultForRow(c.id)
                const issues = validation.get(c.id) ?? []
                return (
                  <motion.div
                    key={c.id}
                    layout={reduced ? false : 'position'}
                    initial={reduced ? false : { opacity: 0, y: -4 }}
                    animate={{ opacity: 1, y: 0 }}
                    exit={reduced ? { opacity: 0 } : { opacity: 0, height: 0, transition: LEAVE }}
                    transition={reduced ? { duration: 0 } : ARRIVE}
                    data-row-id={c.id}
                    className="check-row py-2.5"
                    style={{
                      overflow: 'hidden',
                      ...(issues.length > 0 ? {
                        background: 'var(--err-bg)',
                        boxShadow: 'inset 3px 0 0 var(--err)',
                        paddingLeft: 10,
                      } : {}),
                    }}
                  >
                    <div className="flex items-center justify-between gap-2">
                      <div className="text-[13px] min-w-0 flex flex-wrap items-center gap-x-2" style={{ color: 'var(--silk)' }}>
                        {/* The plain-language label is what the row IS, so it
                            wraps rather than truncating: on a phone column the
                            kind chip and the verdict leave it about 24px. */}
                        <span>{titleOf(checkKind(c.kind)?.kind ?? c.kind)}</span>
                        <code className="text-[10px] shrink-0" style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}>
                          {c.kind}
                        </code>
                        {rowResult && <ResultChip result={rowResult} stale={stale} />}
                      </div>
                      <div className="flex items-center gap-3 shrink-0">
                        {rowResult && (
                          <button
                            type="button"
                            data-testid="row-evidence-toggle"
                            aria-expanded={evidenceOpen.has(c.id)}
                            className="hb-press text-[12px] cursor-pointer underline"
                            style={{ color: 'var(--silk-dim)', background: 'none', border: 'none' }}
                            onClick={() => toggleEvidence(c.id)}
                          >
                            {evidenceOpen.has(c.id) ? 'Hide evidence' : rowResult.passed && !rowResult.invalid ? 'Evidence' : 'Why'}
                          </button>
                        )}
                        <button
                          type="button"
                          className="hb-press text-[12px] cursor-pointer"
                          style={{ color: 'var(--silk-faint)', background: 'none', border: 'none' }}
                          onClick={() => onRemove(c.id)}
                        >
                          remove
                        </button>
                      </div>
                    </div>
                    <div className="mt-2">
                      <ConstraintEditor
                        draft={c}
                        issues={issues}
                        onChange={patch => onUpdate(c.id, patch)}
                      />
                    </div>
                    {/* The missing-values verdict, on the row it judges, in the
                        builder's own field names. */}
                    {issues.length > 0 && (
                      <div data-testid="row-validation" className="mt-1.5 text-[11px]" style={{ color: 'var(--err)' }}>
                        Not run: {issues.map(i => i.message).join(' · ')}
                      </div>
                    )}
                    {/* The result at a glance, on the row it judges: the
                        measured value against its limit, the net or part it
                        is about, and why it was not judged. */}
                    {rowResult && (
                      <AssertionEvidence
                        part="glance"
                        result={rowResult}
                        assumptions={assumptions}
                        inventory={inventory}
                        repeats={repeats[displayRows.indexOf(c)] ?? new Set()}
                        stale={stale}
                      />
                    )}
                    {rowResult && evidenceOpen.has(c.id) && (
                      <AssertionEvidence
                        part="details"
                        result={rowResult}
                        assumptions={assumptions}
                        inventory={inventory}
                        repeats={repeats[displayRows.indexOf(c)] ?? new Set()}
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
