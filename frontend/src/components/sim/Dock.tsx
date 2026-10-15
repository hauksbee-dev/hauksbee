// The live view's secondary instruments (inputs, power, serial, faults, nets,
// solver) as one tabbed dock: one panel open at a time, so the board and its
// scope stay the screen's one working surface. The open tab is remembered per
// browser; a fault count elsewhere can open the Faults tab directly.

const DOCK_KEY = 'hauksbee.simdock'

export interface DockTab {
  id: string
  label: string
  icon: React.ReactNode
  badge?: React.ReactNode
  body: React.ReactNode
}

export function readDockTab(fallback: string): string {
  try {
    return localStorage.getItem(DOCK_KEY) ?? fallback
  } catch {
    return fallback
  }
}

export function saveDockTab(id: string) {
  try { localStorage.setItem(DOCK_KEY, id) } catch { /* per-viewer convenience only */ }
}

export function Dock({ tabs, active, onActivate, header }: {
  tabs: DockTab[]
  active: string
  onActivate: (id: string) => void
  /** A compact strip above the tabs (the MCU state). */
  header?: React.ReactNode
}) {
  const current = tabs.find(t => t.id === active) ?? tabs[0]
  return (
    <div className="flex flex-col h-full min-h-0" data-testid="sim-dock">
      {header}
      <div role="tablist" aria-label="Live instruments" className="flex flex-wrap gap-1 px-2 py-2 shrink-0"
        style={{ borderBottom: '1px solid var(--hairline)' }}>
        {tabs.map(t => {
          const on = t.id === current?.id
          return (
            <button
              key={t.id}
              type="button"
              role="tab"
              aria-selected={on}
              data-testid={`dock-tab-${t.id}`}
              onClick={() => { onActivate(t.id); saveDockTab(t.id) }}
              className="hb-press inline-flex items-center gap-1.5 px-2 rounded-md text-[11px] cursor-pointer"
              style={{
                height: 26,
                background: on ? 'var(--copper-tint-strong)' : 'transparent',
                border: `1px solid ${on ? 'var(--copper-deep)' : 'transparent'}`,
                color: on ? 'var(--copper-hi)' : 'var(--silk-dim)',
              }}
            >
              <span style={{ display: 'inline-flex' }}>{t.icon}</span>
              {t.label}
              {t.badge}
            </button>
          )
        })}
      </div>
      <div role="tabpanel" data-testid={`dock-panel-${current?.id}`} className="flex-1 min-h-0 overflow-y-auto">
        {current?.body}
      </div>
    </div>
  )
}
