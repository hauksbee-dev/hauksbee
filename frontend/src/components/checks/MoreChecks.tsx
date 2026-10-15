import { useEffect, useState } from 'react'
import { api, isAbort } from '../../lib/api'
import type { CatalogEntry } from '../../lib/ci-catalog'
import { PlusIcon } from '../Icons'

/** "More checks (N)": every assertion kind the engine knows, read from
 *  `/api/ci/catalog` (the table `hauksbee-ci explain` prints), so this list, the
 *  CLI and the failure hints cannot disagree. Choosing one appends its example to
 *  the raw TOML; the parent does the writing and selects the first value to fill.
 *  Renders nothing when the server has no catalogue (an older or demo server). */
export function MoreChecks({ onInsert }: {
  onInsert: (entry: CatalogEntry) => void
}) {
  const [kinds, setKinds] = useState<CatalogEntry[]>([])
  const [open, setOpen] = useState(false)
  const [added, setAdded] = useState<CatalogEntry | null>(null)

  useEffect(() => {
    const abort = new AbortController()
    api.ciCatalog(abort.signal)
      .then(res => { if (res.ok && res.kinds) setKinds(res.kinds) })
      .catch(e => { if (!isAbort(e)) setKinds([]) })
    return () => abort.abort()
  }, [])

  if (kinds.length === 0) return null

  return (
    <div className="relative" data-testid="more-checks-root">
      <button
        type="button"
        data-testid="more-checks"
        aria-expanded={open}
        className="hb-btn hb-press px-3 py-1.5 text-[13px] inline-flex items-center gap-1.5"
        onClick={() => setOpen(o => !o)}
      >
        <PlusIcon size={13} /> More checks ({kinds.length})
      </button>
      {added && (
        <div data-testid="more-checks-added" className="mt-2 text-[12px]" style={{ color: 'var(--silk-dim)' }}>
          Added <code style={{ fontFamily: 'var(--font-mono)' }}>{added.kind}</code> to the spec on the right.
          {added.required.length > 0 && <> Fill in {added.required.map((f, i) => (
            <span key={f.name}>
              {i > 0 && ', '}<code style={{ fontFamily: 'var(--font-mono)', color: 'var(--copper)' }}>{f.name}</code>
            </span>
          ))}.</>}
          {added.needs.length > 0 && <> It also needs {added.needs.join(', ')}.</>}
        </div>
      )}
      {open && (
        <ul
          data-testid="more-checks-list"
          className="hb-card view-enter mt-1 overflow-y-auto list-none p-0 m-0"
          style={{ maxWidth: '100%', width: 520, maxHeight: 420, boxShadow: 'var(--shadow-pop)' }}
        >
          {kinds.map(k => (
            <li key={k.kind}>
              <button
                type="button"
                data-testid={`more-check-${k.kind}`}
                onClick={() => { onInsert(k); setAdded(k); setOpen(false) }}
                className="hb-press block w-full text-left px-3 py-2 cursor-pointer"
                style={{ background: 'none', border: 'none' }}
                onMouseEnter={e => { e.currentTarget.style.background = 'var(--copper-tint)' }}
                onMouseLeave={e => { e.currentTarget.style.background = 'none' }}
              >
                <div className="text-[13px] flex flex-wrap items-baseline gap-x-2" style={{ color: 'var(--silk)' }}>
                  {k.title}
                  <code className="text-[10px]" style={{ color: 'var(--silk-faint)', fontFamily: 'var(--font-mono)' }}>{k.kind}</code>
                </div>
                <div className="text-[11px]" style={{ color: 'var(--silk-faint)' }}>{k.summary}</div>
                {k.needs.length > 0 && (
                  <div className="text-[11px] mt-0.5" style={{ color: 'var(--silk-dim)' }}>needs {k.needs.join(', ')}</div>
                )}
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  )
}
