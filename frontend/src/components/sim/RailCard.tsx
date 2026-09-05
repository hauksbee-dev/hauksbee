import { useCallback, useState } from 'react'
import { ChevronDownIcon, ChevronRightIcon } from '../Icons'

// The right-rail card: collapsible, with the open state persisted so the rail
// comes back the way it was left.

const CARD_STATE_KEY = 'hauksbee.simrail'

export interface RailCards {
  isOpen: (id: string, defaultOpen: boolean) => boolean
  toggle: (id: string, open: boolean) => void
}

export function useRailCards(): RailCards {
  const [state, setState] = useState<Record<string, boolean>>(() => {
    try {
      return JSON.parse(localStorage.getItem(CARD_STATE_KEY) ?? '{}') as Record<string, boolean>
    } catch {
      return {}
    }
  })
  const toggle = useCallback((id: string, open: boolean) => {
    setState(prev => {
      const next = { ...prev, [id]: open }
      try { localStorage.setItem(CARD_STATE_KEY, JSON.stringify(next)) } catch { /* non-fatal */ }
      return next
    })
  }, [])
  return { isOpen: (id, defaultOpen) => state[id] ?? defaultOpen, toggle }
}

export function RailCard({ id, title, icon, badge, defaultOpen = true, cards, children }: {
  id: string
  title: string
  icon: React.ReactNode
  badge?: React.ReactNode
  defaultOpen?: boolean
  cards: RailCards
  children: React.ReactNode
}) {
  const open = cards.isOpen(id, defaultOpen)
  return (
    <section
      // shrink-0: the rail is a flex column; without it the cards compress to
      // share the viewport height and every card body gets clipped.
      className="hb-card overflow-hidden shrink-0"
      // scrollSnapAlign start, against the rail's scroll-padding: the rail
      // settles with this card's header as the first thing on screen, never
      // part-way down its body and never with the card above it clipped.
      style={{ borderRadius: 10, scrollSnapAlign: 'start' }}
      data-testid={`rail-${id}`}
    >
      <button
        type="button"
        onClick={() => cards.toggle(id, !open)}
        aria-expanded={open}
        className="hb-press flex items-center gap-2 w-full px-3 cursor-pointer"
        style={{
          height: 36, background: 'none', border: 'none',
          borderBottom: open ? '1px solid var(--rule)' : 'none',
          color: 'var(--silk-dim)', textAlign: 'left',
        }}
      >
        <span style={{ display: 'inline-flex', color: 'var(--copper)' }}>{icon}</span>
        <span className="text-[11px] font-bold tracking-[0.1em] uppercase flex-1" style={{ color: 'var(--silk-dim)' }}>
          {title}
        </span>
        {badge}
        <span style={{ display: 'inline-flex', color: 'var(--silk-faint)' }}>
          {open ? <ChevronDownIcon size={13} /> : <ChevronRightIcon size={13} />}
        </span>
      </button>
      {open && <div>{children}</div>}
    </section>
  )
}
