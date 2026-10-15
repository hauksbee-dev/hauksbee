import { useMemo, useState } from 'react'
import { PlusIcon } from '../Icons'
import {
  blockedBy, KIND_GROUPS, specFacts, useCatalog, useCheckHost, type CatalogKind, type Offer,
} from '../../lib/check-catalog'
import { OfferEditor, useAdder } from './CheckThis'

/** A kind from the menu, as an offer with no subject yet. */
const menuOffer = (entry: CatalogKind): Offer => ({ kind: entry.kind, entry, text: entry.title })

/**
 * "Add a check": every assertion kind the engine knows, grouped, with a filter.
 * Choosing one opens the same in-place editor the board's selection card uses, with
 * the subject left for you to name. A kind whose prerequisite the spec lacks is
 * greyed with the reason and, where one exists, a button that adds it.
 *
 * Without a catalogue (an older server) it shows the builder's own short list.
 */
export function AddCheckMenu({ open, onOpenChange, legacy, onLegacyAdd }: {
  open: boolean
  onOpenChange: (open: boolean) => void
  legacy: ReadonlyArray<{ kind: string; label: string; hint: string }>
  onLegacyAdd: (kind: string) => void
}) {
  const host = useCheckHost()
  const kinds = useCatalog()
  const adder = useAdder(host)
  const [filter, setFilter] = useState('')
  const [picked, setPicked] = useState<CatalogKind | null>(null)
  const facts = specFacts(host)
  const needle = filter.trim().toLowerCase()
  const groups = useMemo(() => {
    if (!kinds) return []
    return KIND_GROUPS.map(([group, members]) => [group, members
      .map(kind => kinds.find(k => k.kind === kind))
      .filter((k): k is CatalogKind => !!k)
      .filter(k => !needle || `${k.title} ${k.summary} ${k.kind}`.toLowerCase().includes(needle))] as const)
      .filter(([, entries]) => entries.length > 0)
  }, [kinds, needle])

  const close = () => { onOpenChange(false); setPicked(null); setFilter('') }
  return (
    <div className="relative">
      <button
        type="button" data-testid="add-check" aria-expanded={open}
        className="hb-btn hb-press px-3 py-1.5 text-[13px] inline-flex items-center gap-1.5"
        onClick={() => (open ? close() : onOpenChange(true))}
      >
        <PlusIcon size={13} /> Add a check
      </button>
      {open && (
        <div
          data-testid="add-check-menu"
          className="hb-card view-enter absolute z-10 mt-1 overflow-y-auto"
          style={{ width: 460, maxWidth: '100%', maxHeight: '70vh', boxShadow: 'var(--shadow-pop)' }}
          onKeyDown={event => { if (event.key === 'Escape' && !picked) close() }}
        >
          {!kinds && legacy.map(k => (
            <button
              key={k.kind} type="button" onClick={() => onLegacyAdd(k.kind)}
              className="hb-press block w-full text-left px-3 py-2 cursor-pointer"
              style={{ background: 'none', border: 'none' }}
            >
              <div className="text-[13px]" style={{ color: 'var(--silk)' }}>{k.label}</div>
              <div className="text-[11px]" style={{ color: 'var(--silk-faint)' }}>{k.hint}</div>
            </button>
          ))}
          {kinds && picked && (
            <div className="flex flex-col gap-2 px-3 py-3">
              <div className="flex items-center justify-between gap-2">
                <b className="text-[13px]" style={{ color: 'var(--silk)' }}>{picked.title}</b>
                <button type="button" className="hb-btn hb-press px-2 py-1 text-[11px]" onClick={() => setPicked(null)}>
                  All kinds
                </button>
              </div>
              <OfferEditor
                offer={menuOffer(picked)} host={host} live={null} defaultNets={host?.nets}
                onCollapse={() => setPicked(null)}
                onAdd={(request, run) => { adder(request, run, picked.kind); close() }}
              />
            </div>
          )}
          {kinds && !picked && (
            <>
              <div className="px-3 pt-3 pb-1">
                <input
                  data-testid="add-check-filter" className="hb-input w-full" placeholder="Filter the kinds"
                  aria-label="Filter the kinds of check" value={filter} autoFocus
                  onChange={event => setFilter(event.currentTarget.value)}
                />
              </div>
              {groups.map(([group, entries]) => (
                <div key={group} data-testid={`menu-group-${group.toLowerCase().replace(/\W+/g, '-')}`}>
                  <div className="px-3 pt-2 pb-0.5 text-[10px] font-bold tracking-[0.14em]" style={{ color: 'var(--silk-faint)' }}>
                    {group.toUpperCase()}
                  </div>
                  {entries.map(entry => {
                    const blocked = blockedBy(entry, facts, kinds)
                    return (
                      <div key={entry.kind} className="px-3 py-1.5" style={{ opacity: blocked ? 0.7 : 1 }}>
                        <button
                          type="button" data-testid={`kind-${entry.kind}`} aria-disabled={blocked ? true : undefined}
                          className="hb-press block w-full text-left cursor-pointer"
                          style={{ background: 'none', border: 'none', padding: 0 }}
                          onClick={() => { if (!blocked) setPicked(entry) }}
                        >
                          <div className="text-[13px]" style={{ color: blocked ? 'var(--silk-faint)' : 'var(--silk)' }}>{entry.title}</div>
                          <div className="text-[11px]" style={{ color: 'var(--silk-faint)' }}>{entry.summary}</div>
                        </button>
                        {blocked && (
                          <div className="mt-1 flex flex-wrap items-center gap-2 text-[11px]" style={{ color: 'var(--silk-faint)' }}>
                            <span>{blocked.reason}</span>
                            {blocked.fix && (
                              <button
                                type="button" data-testid="needs_fix" className="hb-chip hb-press px-2 py-0.5 text-[11px]"
                                onClick={() => {
                                  const fix = blocked.fix!
                                  if (fix.pickFirmware) document.querySelector<HTMLInputElement>('#firmware-file')?.click()
                                  else if (fix.toml) adder({ type: 'assert', kind: blocked.need!.need, toml: fix.toml }, false)
                                }}
                              >
                                {blocked.fix.label}
                              </button>
                            )}
                          </div>
                        )}
                      </div>
                    )
                  })}
                </div>
              ))}
              {groups.length === 0 && (
                <div className="px-3 py-3 text-[12px]" style={{ color: 'var(--silk-faint)' }}>Nothing matches “{filter}”.</div>
              )}
            </>
          )}
        </div>
      )}
    </div>
  )
}
