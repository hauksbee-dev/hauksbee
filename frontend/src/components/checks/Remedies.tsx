import { useEffect, useMemo, useState } from 'react'
import type { EvidenceAssumption, Remedy, RunResponse } from '../../types/report'
import { plural } from '../../lib/report-view'
import { useTermEntry } from '../ui'
import { ChipList, netsOf, partsOf, RowAction } from '../report/ResultLinks'
import { requestModelTab, type ModelTab } from '../WritePart'

// What blocks a run's judgement, as ONE list grouped by cause: every part or
// pin a cause touches is a chip, the cause is explained once, and one row of
// buttons acts on it. The list is the run's whole assumption registry (the
// engine's single list, the one the Board counts too), so the two views agree.
// It sits behind one entry chip ("3 open parts block judgement") and opens as
// a sheet; nothing else on the page repeats it.

/** What a remedy button does in the Checks view. */
export interface RemedyActions {
  /** Power a net: add (or find) its supply row, focus the volts field. */
  addSupply: (net: string, voltsHint?: number) => void
  /** Leave a part out of the simulated circuit as not fitted. */
  markNotFitted: (reference: string) => void
  /** Open the model editor for a part. */
  openModel: (reference: string) => void
  /** Whether the remedy has already been applied to the spec. */
  applied: (remedy: Remedy) => boolean
}

/** One cause and everything it touches. */
export interface CauseGroup {
  kind: string
  term?: string
  items: EvidenceAssumption[]
  parts: string[]
  /** Pin-level targets (`D2/1`), shown as their own chips' captions. */
  pins: string[]
  nets: string[]
}

/** Internal ids (`open-part:C2`, `inferred-pin-role:D2/1`) are for machines;
 *  the visible text keeps only the designator. */
export const withoutIds = (text: string) => text.replace(/\b[a-z][a-z_-]*:(?=[A-Z0-9])/g, '')

/** The part (and pin) an assumption is about, from its remedy or its id. */
function targetOf(a: EvidenceAssumption): { part?: string; pin?: string; net?: string } {
  const r = a.remedy
  if (r && 'net' in r) return { net: r.net }
  const prefixed = /^[a-z_-]+:(.+)$/.exec(a.id)?.[1]
  const raw = r && 'reference' in r ? r.reference : prefixed
  if (!raw) return {}
  const [ref] = raw.split('@')
  const [part, pin] = ref.split('/')
  return pin ? { part, pin: `${part}/${pin}` } : part ? { part } : {}
}

const uniq = (xs: (string | undefined)[]) => [...new Set(xs.filter((x): x is string => !!x))]

/** The registry grouped by cause, in the engine's order. */
export function causeGroups(assumptions: readonly EvidenceAssumption[]): CauseGroup[] {
  const by = new Map<string, EvidenceAssumption[]>()
  for (const a of assumptions) by.set(a.kind, [...(by.get(a.kind) ?? []), a])
  return [...by].map(([kind, items]) => {
    const targets = items.map(targetOf)
    return {
      kind, term: items[0].term, items,
      parts: uniq(targets.map(t => t.part)),
      pins: uniq(targets.map(t => t.pin)),
      nets: uniq(targets.map(t => t.net)),
    }
  })
}

/** The causes that block a run: its whole registry, less pure notes. */
export const blockingCauses = (result: RunResponse): CauseGroup[] =>
  causeGroups(result.assumptions ?? []).filter(g => g.parts.length + g.nets.length > 0)

/** The open-part count, the same number the Board shows. */
export const openPartCount = (groups: readonly CauseGroup[]) =>
  groups.find(g => g.kind === 'open_part')?.parts.length ?? 0

const OPEN_EVENT = 'hb:open-parts'
/** Open the open-parts sheet from anywhere on the Checks page (a row's
 *  "Not judged" line). */
export const showOpenParts = () => window.dispatchEvent(new Event(OPEN_EVENT))

/** The single entry point: a chip with the count that opens the sheet. */
export function OpenPartsEntry({ groups, blocking, onOpen }: { groups: CauseGroup[]; blocking: boolean; onOpen: () => void }) {
  if (groups.length === 0) return null
  const open = openPartCount(groups)
  const what = open > 0 ? plural(open, 'open part') : plural(groups.length, 'open question')
  return (
    <button
      type="button"
      data-testid="open-parts-entry"
      data-count={open}
      onClick={onOpen}
      className="hb-chip hb-press inline-flex items-center gap-1.5 px-2.5 py-1 text-[12px] text-left max-w-full"
      style={{ minHeight: 30, color: blocking ? 'var(--warn-strong)' : undefined }}
    >
      {what}{blocking ? ' block judgement' : ' qualify this run'}
    </button>
  )
}

/** One cause: title, chips, one explanation behind a click, one action row. */
function CauseRow({ group, actions, onModel, onIgnore }: {
  group: CauseGroup
  actions: RemedyActions
  onModel: (ref: string, tab: ModelTab) => void
  onIgnore: () => void
}) {
  const entry = useTermEntry(group.term)
  const [target, setTarget] = useState(group.parts[0] ?? group.nets[0] ?? '')
  const first = group.items[0]
  const remedy = first.remedy
  const partCause = group.parts.length > 0
  const modelled = group.kind !== 'open_part'
  const title = entry?.label ?? withoutIds(first.statement)
  const pick = (options: string[]) => options.length > 1 && (
    <select
      className="hb-input text-[12px]"
      data-testid="cause-target"
      aria-label="Which one the buttons act on"
      value={target}
      onChange={e => setTarget(e.target.value)}
      style={{ height: 26 }}
    >
      {options.map(o => <option key={o} value={o}>{o}</option>)}
    </select>
  )
  return (
    <li className="py-3" data-testid="cause-group" data-kind={group.kind} style={{ borderTop: '1px solid var(--hairline)' }}>
      <div className="flex flex-wrap items-baseline gap-x-2 gap-y-1">
        <span className="text-[13px] font-semibold" style={{ color: 'var(--silk)' }}>{title}</span>
        <span className="text-[12px] tnum" style={{ color: 'var(--silk-faint)' }}>
          {partCause ? plural(group.parts.length, 'part') : plural(group.nets.length, 'net')}
          {group.pins.length > 0 && ` · ${plural(group.pins.length, 'pin')}`}
        </span>
      </div>
      <div className="mt-1 text-[12px]">
        <ChipList entities={[...partsOf(group.parts), ...netsOf(group.nets)]} testId="cause-chips" />
        {group.pins.length > 0 && (
          <span className="ml-1.5" style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}>
            pins {group.pins.join(', ')}
          </span>
        )}
      </div>
      <details className="mt-1 text-[12px]" style={{ color: 'var(--silk-dim)' }}>
        <summary className="cursor-pointer" style={{ color: 'var(--silk-faint)' }}>Why this blocks a verdict</summary>
        <div className="mt-1">{withoutIds(`${first.because} ${first.consequence}`)}</div>
        <div className="mt-1">{withoutIds(first.replacement)}</div>
      </details>
      <div className="mt-2 flex flex-wrap items-center gap-1.5" data-testid="cause-actions">
        {pick(partCause ? group.parts : group.nets)}
        {partCause ? (
          <>
            {modelled && <RowAction testId="cause-extend" primary onClick={() => onModel(target, 'extend')}>Extend model</RowAction>}
            <RowAction testId="cause-draft" primary={!modelled} onClick={() => onModel(target, 'draft')}>Draft from datasheet</RowAction>
            <RowAction testId="cause-write" onClick={() => onModel(target, 'write')}>Write it</RowAction>
            <RowAction testId="cause-dnp" onClick={() => actions.markNotFitted(target)}>
              {actions.applied({ type: 'mark_dnp', reference: target }) ? 'Marked DNP: run again' : 'Mark DNP'}
            </RowAction>
          </>
        ) : remedy?.type === 'add_supply' ? (
          <RowAction testId="cause-supply" primary onClick={() => actions.addSupply(target, remedy.volts_hint)}>
            Add a supply on {target}
          </RowAction>
        ) : null}
        <RowAction testId="cause-ignore" onClick={onIgnore} title="Hide it here for this run; the verdict still counts it">Ignore</RowAction>
      </div>
    </li>
  )
}

/** The sheet: the grouped causes, or, once a model button was pressed, the
 *  model editor alone (one surface at a time). */
export function OpenPartsSheet({ groups, actions, editor, onClose }: {
  groups: CauseGroup[]
  actions: RemedyActions
  /** The model editor the Checks view mounts for the part last asked for. */
  editor?: React.ReactNode
  onClose: () => void
}) {
  const [ignored, setIgnored] = useState<ReadonlySet<string>>(new Set())
  const [editing, setEditing] = useState(false)
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => { if (e.key === 'Escape' && !e.defaultPrevented) onClose() }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  }, [onClose])
  const shown = groups.filter(g => !ignored.has(g.kind))
  const onModel = (ref: string, tab: ModelTab) => {
    requestModelTab(tab)
    actions.openModel(ref)
    setEditing(true)
  }
  return (
    <div
      className="fixed inset-0 z-[80] flex justify-end"
      style={{ background: 'color-mix(in srgb, var(--canvas) 72%, transparent)' }}
      role="presentation"
      onMouseDown={e => { if (e.target === e.currentTarget) onClose() }}
    >
      <section
        role="dialog"
        aria-modal="true"
        aria-label="What blocks judgement"
        data-testid="open-parts-sheet"
        className="h-full w-full overflow-y-auto px-4 py-4"
        style={{ maxWidth: 560, background: 'var(--surface)', boxShadow: 'var(--shadow-pop)', borderLeft: '1px solid var(--hairline)' }}
      >
        <div className="flex items-center justify-between gap-2">
          {editing && editor ? (
            <button type="button" data-testid="open-parts-back" className="hb-btn hb-press px-2.5 text-[12px]" style={{ height: 28 }} onClick={() => setEditing(false)}>
              ← All causes
            </button>
          ) : (
            <h2 className="text-[15px] font-semibold" style={{ color: 'var(--silk)', margin: 0 }}>What blocks judgement</h2>
          )}
          <button type="button" className="hb-btn hb-press px-2.5 text-[12px]" style={{ height: 28 }} onClick={onClose} data-testid="open-parts-close">
            Close
          </button>
        </div>
        {editing && editor ? editor : (
          <>
            <p className="mt-1 text-[12px]" style={{ color: 'var(--silk-dim)' }}>
              Each fix edits the spec or saves a model; run the checks again to see the new verdict.
            </p>
            <ul className="list-none p-0 m-0 mt-2" data-testid="open-parts-list">
              {shown.map(g => (
                <CauseRow key={g.kind} group={g} actions={actions} onModel={onModel}
                  onIgnore={() => setIgnored(s => new Set([...s, g.kind]))} />
              ))}
            </ul>
            {ignored.size > 0 && (
              <button type="button" className="mt-2 text-[12px] underline" data-testid="open-parts-unignore"
                style={{ color: 'var(--copper-hi)', background: 'none', border: 'none', padding: 0 }}
                onClick={() => setIgnored(new Set())}>
                Show {plural(ignored.size, 'ignored cause')}
              </button>
            )}
          </>
        )}
      </section>
    </div>
  )
}

/** The entry chip and its sheet, wired to the run. Listens for
 *  {@link showOpenParts} so a row's "Not judged" line opens the same sheet. */
export function OpenParts({ result, blocking, actions, editor }: {
  result: RunResponse
  blocking: boolean
  actions?: RemedyActions
  editor?: React.ReactNode
}) {
  const groups = useMemo(() => blockingCauses(result), [result])
  const [open, setOpen] = useState(false)
  useEffect(() => {
    const show = () => setOpen(true)
    window.addEventListener(OPEN_EVENT, show)
    return () => window.removeEventListener(OPEN_EVENT, show)
  }, [])
  if (!actions) return null
  return (
    <>
      <OpenPartsEntry groups={groups} blocking={blocking} onOpen={() => setOpen(true)} />
      {open && <OpenPartsSheet groups={groups} actions={actions} editor={editor} onClose={() => setOpen(false)} />}
    </>
  )
}
