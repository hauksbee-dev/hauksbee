import type { EvidenceAssumption, Remedy, RunResponse } from '../../types/report'

// One-click remedies for a run that could not be judged. The engine ships each
// gap's fix as data (`assumption.remedy`); this turns it into the button that
// applies it, and leaves anything it has no button for as the engine's own
// sentence.

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

export interface RemedyItem {
  key: string
  remedy: Remedy
  assumption: EvidenceAssumption
}

const target = (r: Remedy): string =>
  'net' in r ? r.net : 'reference' in r ? r.reference : 'id' in r ? r.id : 'file' in r ? r.file : ''

/** The distinct typed remedies behind the results that were not judged, in the
 *  order the engine listed them. A result's own evidence names the assumptions
 *  that undermine it; when none does (a run-level refusal) every typed remedy
 *  in the registry is offered. */
export function blockingRemedies(result: RunResponse): RemedyItem[] {
  const registry = result.assumptions ?? []
  const undermining = new Set(
    (result.results ?? []).filter(x => x.invalid).flatMap(x => x.evidence?.assumptions ?? []),
  )
  const pool = undermining.size > 0 ? registry.filter(a => undermining.has(a.id)) : registry
  const seen = new Set<string>()
  const items: RemedyItem[] = []
  for (const assumption of pool) {
    const remedy = assumption.remedy
    if (!remedy || remedy.type === 'other') continue
    const key = `${remedy.type}:${target(remedy)}`
    if (seen.has(key)) continue
    seen.add(key)
    items.push({ key, remedy, assumption })
  }
  return items
}

/** The button text for a remedy that has one, or null when only the engine's
 *  sentence applies. */
function label(r: Remedy): string | null {
  switch (r.type) {
    case 'add_supply': return `Add a supply on ${r.net}`
    case 'add_model': return `Write or draft a model for ${r.reference}`
    case 'mark_dnp': return `${r.reference} is not fitted: leave it out`
    case 'set_part_number': return `Copy the fix for ${r.reference}`
    default: return null
  }
}

function RemedyRow({ item, actions }: { item: RemedyItem; actions: RemedyActions }) {
  const { remedy, assumption } = item
  const text = label(remedy)
  const done = actions.applied(remedy)
  const run = () => {
    if (remedy.type === 'add_supply') actions.addSupply(remedy.net, remedy.volts_hint)
    else if (remedy.type === 'add_model') actions.openModel(remedy.reference)
    else if (remedy.type === 'mark_dnp') actions.markNotFitted(remedy.reference)
    else if (remedy.type === 'set_part_number') {
      // The part number is a design-file edit, so the click hands over the
      // sentence to act on rather than pretending to apply it.
      void navigator.clipboard?.writeText(assumption.replacement).catch(() => undefined)
    }
  }
  return (
    <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1.5" data-testid="remedy-row">
      {text ? (
        <button
          type="button"
          data-testid={`remedy-${remedy.type.replaceAll('_', '-')}`}
          disabled={done}
          onClick={run}
          className="hb-btn-primary hb-press px-3 py-1.5 text-[12px]"
        >
          {done ? 'Done: run the checks again' : text}
        </button>
      ) : null}
      <span className="text-[12px] min-w-0" style={{ color: 'var(--silk-dim)', overflowWrap: 'anywhere' }}>
        {assumption.replacement}
      </span>
    </div>
  )
}

/** Every blocking remedy, one row each. */
export function RemedyList({ items, actions }: { items: RemedyItem[]; actions: RemedyActions }) {
  return (
    <div data-testid="remedy-list">
      {items.map(item => <RemedyRow key={item.key} item={item} actions={actions} />)}
    </div>
  )
}
